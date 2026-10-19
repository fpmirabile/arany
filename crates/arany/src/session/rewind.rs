use super::{Checkpoint, CheckpointStatus, ReplayError, RunId, SessionView, WorkspaceIdentity};
use crate::session::worker::{ChangesetSource, WorkerBaseVersion, WorkerDataFacts};
use crate::tools::{ToolCall, ToolDisposition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const MAX_REWIND_FILES: usize = 2_048;
pub const MAX_REWIND_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_REWIND_SELECTIONS: usize = 64;
pub const MAX_REWIND_CONFLICTS: usize = 32;
pub const MAX_REWIND_EVENT_BYTES: usize = 64 * 1024;
const MAX_PLANNER_PUBLICATIONS: usize = 10_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointTarget {
    Run { run_id: RunId, sequence: u64 },
    Rewind { rewind_id: Uuid, sequence: u64 },
}
impl CheckpointTarget {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Run { sequence, .. } | Self::Rewind { sequence, .. } => *sequence,
        }
    }
    fn valid(&self) -> bool {
        self.sequence() > 0
            && match self {
                Self::Run { run_id, .. } => run_id.to_string().parse::<RunId>().is_ok(),
                Self::Rewind { rewind_id, .. } => valid_id(*rewind_id),
            }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileState {
    pub sha256: String,
    pub bytes: u32,
    pub executable: bool,
}
impl FileState {
    fn valid(&self) -> bool {
        crate::tools::types::valid_digest(&self.sha256) && self.bytes <= 1024 * 1024
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePlan {
    pub path: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub restore_to: Option<FileState>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub expected_current: Option<FileState>,
    pub history_drift: bool,
    pub unavailable: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindPlan {
    pub files: Vec<FilePlan>,
    pub blockers: Vec<u64>,
    pub non_rewindable: Vec<(u64, NonRewindable)>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NonRewindable {
    UnguardedCommand,
    Mcp,
    Cache,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overwrite {
    pub path: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub observed: Option<FileState>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindStarted {
    pub rewind_id: Uuid,
    pub target: CheckpointTarget,
    pub head_sequence: u64,
    pub prefix_digest: [u8; 32],
    pub plan_digest: [u8; 32],
    pub file_count: u32,
    pub restored_bytes: u64,
    pub kept: Vec<String>,
    pub overwrite: Vec<Overwrite>,
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub policy_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub expires_at_ms: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindOutcome {
    Restored,
    Conflict,
    Denied,
    Failed,
    Cancelled,
    Uncertain,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindAttestation {
    pub plan_digest: [u8; 32],
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub enforcement_digest: [u8; 32],
    pub receipt_digest: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindFinished {
    pub rewind_id: Uuid,
    pub outcome: RewindOutcome,
    #[serde(deserialize_with = "Option::deserialize")]
    pub attestation: Option<RewindAttestation>,
    pub conflicts: Vec<String>,
    pub conflict_count: u32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub checkpoint: Option<Checkpoint>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindView {
    pub started: RewindStarted,
    pub started_sequence: u64,
    pub finished: Option<RewindFinished>,
    pub finished_sequence: Option<u64>,
    pub files: Vec<FilePlan>,
    pub sealed: bool,
}
impl RewindView {
    pub fn outcome(&self) -> RewindOutcome {
        self.finished
            .as_ref()
            .map_or(RewindOutcome::Uncertain, |f| f.outcome)
    }
    pub fn history_fact(&self) -> String {
        let mut fact = format!(
            "Workspace rewind {}: {:?}; {} files; target sequence {} (untrusted history, never permission).",
            self.started.rewind_id,
            self.outcome(),
            self.files.len(),
            self.started.target.sequence()
        );
        for file in self.files.iter().take(8) {
            let path = serde_json::to_string(&file.path).expect("rewind path");
            if fact.len() + 1 + path.len() > 4096 {
                break;
            }
            fact.push(' ');
            fact.push_str(&path);
        }
        fact
    }
}

#[derive(Clone, Debug, Default)]
pub struct RewindState {
    pub checkpoints: Vec<CheckpointTarget>,
    pub operations: Vec<RewindView>,
    pub(crate) prefix: super::lineage::PrefixHasher,
}

impl PartialEq for RewindState {
    fn eq(&self, other: &Self) -> bool {
        self.checkpoints == other.checkpoints && self.operations == other.operations
    }
}
impl Eq for RewindState {}

pub(crate) fn valid_id(id: Uuid) -> bool {
    id.get_version() == Some(uuid::Version::SortRand)
}
fn sorted_paths<'a>(paths: impl Iterator<Item = &'a str>) -> bool {
    let mut prior = None;
    for path in paths {
        if !crate::tools::types::valid_relative(path, false)
            || path.len() > 512
            || prior.is_some_and(|p| p >= path)
        {
            return false;
        }
        prior = Some(path);
    }
    true
}
fn fits(value: &impl Serialize) -> bool {
    serde_json::to_vec(value).is_ok_and(|b| b.len() <= MAX_REWIND_EVENT_BYTES)
}
impl RewindStarted {
    pub(crate) fn valid(&self) -> bool {
        valid_id(self.rewind_id)
            && self.target.valid()
            && self.target.sequence() <= self.head_sequence
            && self.prefix_digest != [0; 32]
            && self.plan_digest != [0; 32]
            && (1..=MAX_REWIND_FILES as u32).contains(&self.file_count)
            && self.restored_bytes <= MAX_REWIND_BYTES
            && self.workspace_inode != 0
            && self.policy_digest != [0; 32]
            && self.enforcement_digest != [0; 32]
            && self.expires_at_ms > 0
            && self.kept.len() + self.overwrite.len() <= MAX_REWIND_SELECTIONS
            && sorted_paths(self.kept.iter().map(String::as_str))
            && sorted_paths(self.overwrite.iter().map(|o| o.path.as_str()))
            && self.overwrite.iter().all(|o| {
                o.observed.as_ref().is_none_or(FileState::valid) && !self.kept.contains(&o.path)
            })
            && fits(self)
    }
}
impl RewindFinished {
    pub(crate) fn valid(&self) -> bool {
        valid_id(self.rewind_id)
            && self.conflicts.len() <= MAX_REWIND_CONFLICTS
            && self.conflict_count <= MAX_REWIND_FILES as u32
            && self.conflicts.len() == (self.conflict_count as usize).min(MAX_REWIND_CONFLICTS)
            && (self.conflict_count == 0 || self.outcome == RewindOutcome::Conflict)
            && sorted_paths(self.conflicts.iter().map(String::as_str))
            && self.checkpoint.as_ref().is_none_or(Checkpoint::valid)
            && (self.outcome == RewindOutcome::Restored
                || self.outcome == RewindOutcome::Uncertain
                || self.attestation.is_none() && self.checkpoint.is_none())
            && (self.outcome != RewindOutcome::Restored || self.attestation.is_some())
            && self
                .attestation
                .as_ref()
                .is_none_or(|a| a.receipt_digest != [0; 32])
            && fits(self)
    }
}

impl RewindPlan {
    pub fn select(&self, kept: &[String], overwrite: &[Overwrite]) -> Result<Self, ReplayError> {
        if kept.len() + overwrite.len() > MAX_REWIND_SELECTIONS
            || !sorted_paths(kept.iter().map(String::as_str))
            || !sorted_paths(overwrite.iter().map(|o| o.path.as_str()))
            || kept
                .iter()
                .any(|p| !self.files.iter().any(|f| &f.path == p))
            || overwrite.iter().any(|o| {
                kept.contains(&o.path)
                    || !self.files.iter().any(|f| f.path == o.path)
                    || !o.observed.as_ref().is_none_or(FileState::valid)
            })
        {
            return Err(ReplayError::InvalidTransition);
        }
        let mut selected = self.clone();
        selected.files.retain(|f| !kept.contains(&f.path));
        for file in &mut selected.files {
            if let Some(o) = overwrite.iter().find(|o| o.path == file.path) {
                file.expected_current = o.observed.clone();
                file.history_drift = false;
            }
        }
        if !selected.within_limits() {
            return Err(ReplayError::InvalidTransition);
        }
        Ok(selected)
    }
    pub fn restored_bytes(&self) -> u64 {
        self.files
            .iter()
            .filter_map(|f| f.restore_to.as_ref())
            .map(|v| u64::from(v.bytes))
            .sum()
    }
    pub fn transfer_bytes(&self) -> u64 {
        // Base64 needs no JSON escaping: empty content plus encoded lengths is exact.
        let version = |v: &FileState| crate::tools::FileVersion {
            sha256: v.sha256.clone(),
            content: String::new(),
            executable: v.executable,
        };
        let skeleton = crate::session::worker::WorkerChangeset {
            files: self
                .files
                .iter()
                .map(|f| crate::tools::FileChange {
                    path: f.path.clone(),
                    before: f.expected_current.as_ref().map(version),
                    after: f.restore_to.as_ref().map(version),
                })
                .collect(),
        };
        serde_json::to_vec(&skeleton)
            .expect("rewind transfer metadata")
            .len() as u64
            + self
                .files
                .iter()
                .flat_map(|f| [&f.expected_current, &f.restore_to])
                .filter_map(|v| v.as_ref())
                .map(|v| u64::from(v.bytes).div_ceil(3) * 4)
                .sum::<u64>()
    }
    fn within_limits(&self) -> bool {
        self.files.len() <= MAX_REWIND_FILES
            && self.restored_bytes() <= MAX_REWIND_BYTES
            && self
                .files
                .iter()
                .filter_map(|f| f.expected_current.as_ref())
                .map(|v| u64::from(v.bytes))
                .sum::<u64>()
                <= MAX_REWIND_BYTES
            && self.transfer_bytes() <= crate::session::worker::WORKER_DATA_BYTES as u64
    }
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"arany-rewind-plan-v1\0");
        hash.update(serde_json::to_vec(self).expect("bounded rewind plan"));
        hash.finalize().into()
    }
}

fn worker_version(v: &WorkerBaseVersion) -> Result<Option<FileState>, ReplayError> {
    match v {
        WorkerBaseVersion::Absent => Ok(None),
        WorkerBaseVersion::File {
            sha256,
            bytes,
            executable,
        } => Ok(Some(FileState {
            sha256: sha256.clone(),
            bytes: *bytes,
            executable: *executable,
        })),
        WorkerBaseVersion::Directory => Err(ReplayError::InvalidTransition),
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilePublicationResult {
    pub sha256: String,
    pub bytes: u32,
    pub executable: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    pub before: Option<FileState>,
}
pub(crate) fn publication_files(
    run: &super::RunView,
    o: &crate::tools::ToolObservation,
) -> Result<Vec<FilePlan>, ReplayError> {
    let file = |path: String, before, after| FilePlan {
        path,
        restore_to: before,
        expected_current: after,
        history_drift: false,
        unavailable: false,
    };
    let version = |v: &crate::tools::FileVersion| -> Result<FileState, ReplayError> {
        let bytes = v
            .bytes_with_limit(1024 * 1024)
            .ok_or(ReplayError::InvalidPayload)?;
        Ok(FileState {
            sha256: v.sha256.clone(),
            bytes: bytes.len() as u32,
            executable: v.executable,
        })
    };
    match &o.intent.call {
        ToolCall::Write { path, .. } | ToolCall::Edit { path, .. } => {
            let p: FilePublicationResult =
                serde_json::from_str(&o.output).map_err(|_| ReplayError::InvalidPayload)?;
            let expected = match &o.intent.call {
                ToolCall::Write {
                    expected_digest, ..
                } => expected_digest.as_deref(),
                ToolCall::Edit {
                    expected_digest, ..
                } => Some(expected_digest.as_str()),
                _ => unreachable!(),
            };
            let after = FileState {
                sha256: p.sha256,
                bytes: p.bytes,
                executable: p.executable,
            };
            if !after.valid()
                || p.before.as_ref().is_some_and(|v| !v.valid())
                || p.before.as_ref().map(|v| v.sha256.as_str()) != expected
            {
                return Err(ReplayError::InvalidPayload);
            }
            Ok(vec![file(path.clone(), p.before, Some(after))])
        }
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt { changeset, .. },
        } => changeset
            .files
            .iter()
            .map(|f| {
                Ok(file(
                    f.path.clone(),
                    f.before.as_ref().map(version).transpose()?,
                    f.after.as_ref().map(version).transpose()?,
                ))
            })
            .collect(),
        ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover { data, .. },
        } => {
            let facts = run
                .workers
                .data
                .iter()
                .find(|s| &s.reference == data)
                .ok_or(ReplayError::InvalidTransition)?;
            let WorkerDataFacts::Changes(files) = &facts.facts else {
                return Err(ReplayError::InvalidTransition);
            };
            files
                .iter()
                .map(|f| {
                    Ok(file(
                        f.path.clone(),
                        worker_version(&f.before)?,
                        worker_version(&f.after)?,
                    ))
                })
                .collect()
        }
        _ => Err(ReplayError::InvalidTransition),
    }
}

enum Publication<'a> {
    Tool(&'a super::RunView, &'a super::ToolView),
    Rewind(&'a RewindView),
}
impl Publication<'_> {
    fn changes(self, pruned_through: u64) -> Result<(Vec<FilePlan>, bool), ReplayError> {
        match self {
            Self::Tool(run, tool) => Ok((
                publication_files(
                    run,
                    tool.observation
                        .as_ref()
                        .ok_or(ReplayError::InvalidTransition)?,
                )?,
                run.accepted_sequence <= pruned_through
                    || tool
                        .checkpoint
                        .as_ref()
                        .is_none_or(|c| c.status == CheckpointStatus::Skipped),
            )),
            Self::Rewind(r) => Ok((
                r.files
                    .iter()
                    .map(|f| FilePlan {
                        path: f.path.clone(),
                        restore_to: f.expected_current.clone(),
                        expected_current: f.restore_to.clone(),
                        history_drift: false,
                        unavailable: false,
                    })
                    .collect(),
                r.started_sequence <= pruned_through
                    || r.finished
                        .as_ref()
                        .and_then(|f| f.checkpoint.as_ref())
                        .is_none_or(|c| c.status == CheckpointStatus::Skipped),
            )),
        }
    }
}

pub fn plan(view: &SessionView, target: &CheckpointTarget) -> Result<RewindPlan, ReplayError> {
    if !view.rewinds.checkpoints.contains(target) || target.sequence() > view.last_sequence {
        return Err(ReplayError::InvalidTransition);
    }
    let pruned_through = view
        .runs
        .iter()
        .flat_map(|run| &run.tools)
        .filter_map(|tool| tool.checkpoint.as_ref())
        .chain(
            view.rewinds
                .operations
                .iter()
                .filter_map(|r| r.finished.as_ref().and_then(|f| f.checkpoint.as_ref())),
        )
        .filter_map(|checkpoint| checkpoint.pruned_before_run)
        .max()
        .and_then(|pruned| {
            view.rewinds
                .checkpoints
                .iter()
                .find_map(|checkpoint| match checkpoint {
                    CheckpointTarget::Run { run_id, sequence } if *run_id == pruned => {
                        Some(*sequence)
                    }
                    _ => None,
                })
        })
        .unwrap_or(0);
    let mut steps = Vec::new();
    let mut blockers = Vec::new();
    let mut non_rewindable = Vec::new();
    for run in &view.runs {
        let started = view.rewinds.checkpoints.iter().find_map(|c| match c {
            CheckpointTarget::Run { run_id, sequence } if *run_id == run.id => Some(*sequence),
            _ => None,
        });
        let Some(started) = started.filter(|s| *s >= target.sequence()) else {
            continue;
        };
        for t in &run.tools {
            if super::checkpoint::publication(&t.intent) {
                match &t.observation {
                    None => blockers.push(t.finished_sequence.unwrap_or(started)),
                    Some(o) if o.disposition == ToolDisposition::Uncertain => {
                        blockers.push(t.finished_sequence.unwrap_or(started))
                    }
                    Some(o) if o.disposition == ToolDisposition::Succeeded => {
                        steps.push((
                            t.finished_sequence.ok_or(ReplayError::InvalidTransition)?,
                            Publication::Tool(run, t),
                        ));
                    }
                    _ => {}
                }
            }
            let seq = t.finished_sequence.unwrap_or(started);
            if t.ran_unguarded() {
                non_rewindable.push((seq, NonRewindable::UnguardedCommand));
            }
            let Some(o) = &t.observation else {
                continue;
            };
            if o.attestation.is_some() || o.disposition == ToolDisposition::Uncertain {
                match o.intent.call {
                    ToolCall::McpCall { .. } => non_rewindable.push((seq, NonRewindable::Mcp)),
                    ToolCall::Mkdir { .. } => non_rewindable.push((seq, NonRewindable::Directory)),
                    _ => {}
                }
            }
            if matches!(
                o.cache_update,
                crate::tools::CacheUpdate::Published | crate::tools::CacheUpdate::Uncertain
            ) {
                non_rewindable.push((seq, NonRewindable::Cache));
            }
        }
    }
    for r in &view.rewinds.operations {
        if r.started_sequence < target.sequence() {
            continue;
        }
        if r.outcome() == RewindOutcome::Uncertain {
            blockers.push(r.started_sequence);
        }
        if r.outcome() == RewindOutcome::Restored {
            steps.push((
                r.finished_sequence.ok_or(ReplayError::InvalidTransition)?,
                Publication::Rewind(r),
            ));
        }
    }
    if steps.len() + blockers.len() > MAX_PLANNER_PUBLICATIONS
        || non_rewindable.len() > 3 * MAX_PLANNER_PUBLICATIONS
    {
        return Err(ReplayError::InvalidTransition);
    }
    steps.sort_by_key(|s| s.0);
    let mut files: BTreeMap<String, FilePlan> = BTreeMap::new();
    for (_, publication) in steps {
        let (changes, unavailable) = publication.changes(pruned_through)?;
        for mut change in changes {
            if !crate::tools::types::valid_relative(&change.path, false)
                || change.restore_to.as_ref().is_some_and(|v| !v.valid())
                || change.expected_current.as_ref().is_some_and(|v| !v.valid())
            {
                return Err(ReplayError::InvalidTransition);
            }
            if let Some(prior) = files.get_mut(&change.path) {
                prior.history_drift |= prior.expected_current != change.restore_to;
                prior.expected_current = change.expected_current;
            } else {
                if files.len() == MAX_REWIND_FILES * MAX_PLANNER_PUBLICATIONS {
                    return Err(ReplayError::InvalidTransition);
                }
                change.unavailable = unavailable && change.restore_to.is_some();
                files.insert(change.path.clone(), change);
            }
        }
    }
    let files: Vec<_> = files
        .into_values()
        .filter(|f| f.restore_to != f.expected_current)
        .collect();
    blockers.sort_unstable();
    blockers.dedup();
    non_rewindable.sort_by_key(|s| s.0);
    let p = RewindPlan {
        files,
        blockers,
        non_rewindable,
    };
    if !p.within_limits() {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(p)
}

pub(crate) fn workspace(start: &RewindStarted) -> WorkspaceIdentity {
    WorkspaceIdentity {
        device: start.workspace_device,
        inode: start.workspace_inode,
    }
}

pub(crate) fn invalidates(contract: &super::GoalPlanContract, files: &[FilePlan]) -> bool {
    match &contract.success_criterion {
        super::GoalSuccessCriterion::WorkspaceFileDigest { path, .. }
        | super::GoalSuccessCriterion::WorkspaceFileContent { path, .. } => {
            files.iter().any(|f| &f.path == path)
        }
        super::GoalSuccessCriterion::ToolCallSucceeded { call } => {
            call.mutates() || call.is_process()
        }
    }
}

pub(crate) fn goal_blocked(view: &SessionView, goal: &super::GoalView) -> bool {
    let after = goal
        .run_ids
        .iter()
        .filter_map(|id| view.runs.iter().find(|r| r.id == *id))
        .map(|r| r.accepted_sequence)
        .max()
        .unwrap_or(goal.started_sequence);
    view.rewinds
        .operations
        .iter()
        .any(|r| r.started_sequence > after && r.outcome() == RewindOutcome::Uncertain)
}

pub(crate) fn history_facts(view: &SessionView, after: u64, through: u64) -> Option<String> {
    let mut selected: Vec<_> = view
        .rewinds
        .operations
        .iter()
        .rev()
        .filter(|r| r.started_sequence > after && r.started_sequence <= through)
        .take(9)
        .collect();
    if selected.is_empty() {
        return None;
    }
    let omitted = selected.len() > 8;
    selected.truncate(8);
    selected.reverse();
    let mut facts = String::from("Workspace rewind facts (untrusted history):");
    for r in selected {
        facts.push('\n');
        facts.push_str(&r.history_fact());
    }
    if omitted {
        facts.push_str("\nOlder Workspace rewind facts omitted; inspect Session history.");
    }
    Some(facts)
}

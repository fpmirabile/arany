use super::{AgentRunId, GoalSuccessCriterion, ReplayError};
use crate::tools::{FileChange, ToolCall};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const WORKER_CHUNK_BYTES: usize = 45 * 1024;
pub const WORKER_DATA_BYTES: usize = 48 * 1024 * 1024;
pub const WORKER_MANIFEST_BYTES: usize = 3 * 1024 * 1024;
pub const WORKER_CONTENT_BYTES: usize = 32 * 1024 * 1024;
pub const WORKER_FILES: usize = 2048;
pub const WORKER_CHUNKS: usize = WORKER_DATA_BYTES.div_ceil(WORKER_CHUNK_BYTES);
pub const WORKER_MANIFEST_CHUNKS: usize = WORKER_MANIFEST_BYTES.div_ceil(WORKER_CHUNK_BYTES);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMode {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTool {
    List,
    Read,
    Search,
    Mkdir,
    Write,
    Edit,
    Skill,
    McpList,
    Command,
}

impl WorkerTool {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
            Self::Search => "search",
            Self::Mkdir => "mkdir",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Skill => "skill",
            Self::McpList => "mcp_list",
            Self::Command => "command",
        }
    }

    pub(crate) fn of(call: &ToolCall) -> Option<Self> {
        Some(match call {
            ToolCall::List { .. } => Self::List,
            ToolCall::Read { .. } => Self::Read,
            ToolCall::Search { .. } => Self::Search,
            ToolCall::Mkdir { .. } => Self::Mkdir,
            ToolCall::Write { .. } => Self::Write,
            ToolCall::Edit { .. } => Self::Edit,
            ToolCall::Skill { .. } => Self::Skill,
            ToolCall::McpList { .. } => Self::McpList,
            ToolCall::Command { .. } => Self::Command,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerGrant {
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
    pub tools: Vec<WorkerTool>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub commands: Vec<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub permission_mode: Option<crate::tools::permissions::PermissionMode>,
}

impl WorkerGrant {
    pub(crate) fn valid(&self) -> bool {
        valid_commands(&self.commands)
            && (self.commands.is_empty() || self.tools.contains(&WorkerTool::Command))
            && !self.read_paths.is_empty()
            && self.read_paths.len() <= 64
            && self
                .read_paths
                .iter()
                .all(|p| crate::tools::types::valid_relative(p, true))
            && (self.write_paths == ["."] || valid_leases(&self.write_paths))
            && !self.tools.is_empty()
            && self.tools.len() <= 9
            && self
                .tools
                .iter()
                .enumerate()
                .all(|(i, t)| !self.tools[..i].contains(t))
            && self
                .tools
                .iter()
                .any(|t| matches!(t, WorkerTool::Write | WorkerTool::Edit))
            && self.write_paths.iter().all(|p| within(p, &self.read_paths))
            && [&self.skills, &self.mcp_servers].iter().all(|names| {
                names.len() <= 64
                    && names
                        .iter()
                        .enumerate()
                        .all(|(i, n)| crate::tools::types::valid_name(n) && !names[..i].contains(n))
            })
    }

    pub(crate) fn within(&self, parent: &Self) -> bool {
        self.valid()
            && self
                .read_paths
                .iter()
                .all(|p| within(p, &parent.read_paths))
            && self
                .write_paths
                .iter()
                .all(|p| within(p, &parent.write_paths))
            && self.commands.iter().all(|c| parent.commands.contains(c))
            && self.tools.iter().all(|t| parent.tools.contains(t))
            && self.skills.iter().all(|s| parent.skills.contains(s))
            && self
                .mcp_servers
                .iter()
                .all(|s| parent.mcp_servers.contains(s))
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        WorkerTool::of(call).is_some_and(|t| self.tools.contains(&t))
            && match call {
                ToolCall::List { path }
                | ToolCall::Read { path, .. }
                | ToolCall::Search { path, .. } => within(path, &self.read_paths),
                ToolCall::Mkdir { path }
                | ToolCall::Write { path, .. }
                | ToolCall::Edit { path, .. } => {
                    writable_path(path) && within(path, &self.write_paths)
                }
                ToolCall::Skill { name, .. } => self.skills.contains(name),
                ToolCall::McpList { server } => self.mcp_servers.contains(server),
                ToolCall::Command { program, cwd, .. } => {
                    self.commands.contains(program) && (cwd == "." || within(cwd, &self.read_paths))
                }
                _ => false,
            }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPolicy {
    pub parent_grant: WorkerGrant,
    pub concurrency: u8,
    pub max_turns: u8,
    pub max_tools: u8,
    pub data_bytes: u32,
}
impl WorkerPolicy {
    pub(crate) fn retained_bytes(&self) -> usize {
        (64 * 1024 * 1024).min(256 * 1024 * 1024 / usize::from(self.concurrency))
    }

    pub(crate) fn valid(&self) -> bool {
        self.parent_grant.valid()
            && (1..=8).contains(&self.concurrency)
            && (1..=17).contains(&self.max_turns)
            && self.max_tools <= 16
            && self.data_bytes > 0
            && self.data_bytes as usize <= WORKER_DATA_BYTES
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorAssignment {
    pub definition: String,
    pub definition_digest: [u8; 32],
    pub mission: String,
    pub direction: String,
    pub lease: Vec<String>,
    pub commands: Vec<String>,
    pub criterion: GoalSuccessCriterion,
    pub mode: WorkerMode,
    pub max_turns: u8,
    pub wave: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    pub resumes: Option<AgentRunId>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub supersedes: Option<AgentRunId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildAssignment {
    Researcher {
        objective: String,
        #[serde(deserialize_with = "Option::deserialize")]
        definition: Option<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        profile: Option<String>,
    },
    Editor {
        assignment: Box<EditorAssignment>,
    },
}

impl ChildAssignment {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Researcher {
                objective,
                definition,
                profile,
            } => {
                !objective.is_empty()
                    && objective.len() <= 2048
                    && profile
                        .as_deref()
                        .is_none_or(crate::provider::valid_profile_name)
                    && definition
                        .as_ref()
                        .is_none_or(|n| crate::tools::types::valid_name(n))
            }
            Self::Editor { assignment: a } => {
                crate::tools::types::valid_name(&a.definition)
                    && a.definition_digest != [0; 32]
                    && !a.mission.is_empty()
                    && a.mission.len() <= 2048
                    && a.direction.len() <= 2048
                    && valid_leases(&a.lease)
                    && valid_commands(&a.commands)
                    && (1..=17).contains(&a.max_turns)
                    && (1..=8).contains(&a.wave)
                    && [&a.resumes, &a.supersedes]
                        .iter()
                        .all(|id| id.is_none_or(|id| id.valid()))
                    && criterion_valid(&a.criterion)
            }
        }
    }
}

pub(crate) fn criterion_valid(criterion: &GoalSuccessCriterion) -> bool {
    serde_json::to_vec(criterion).is_ok_and(|b| b.len() <= 8192)
        && match criterion {
            GoalSuccessCriterion::ToolCallSucceeded { call } => {
                call.valid() && call.facts().goal_success
            }
            GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
                crate::tools::types::valid_relative(path, false)
                    && crate::tools::types::valid_digest(sha256)
            }
            GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                crate::tools::types::valid_relative(path, false) && content.len() <= 8 * 1024
            }
        }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerAssignment {
    pub request: EditorAssignment,
    pub grant: WorkerGrant,
    pub policy_digest: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    pub permissions_digest: Option<[u8; 32]>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub cache_generation: Option<[u8; 32]>,
    pub base: WorkerDataRef,
    pub data_bytes: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectTarget {
    Workspace,
    WorkerView {
        assignment_id: AgentRunId,
        base_digest: [u8; 32],
        revision: u32,
        #[serde(deserialize_with = "Option::deserialize")]
        cache_generation: Option<[u8; 32]>,
    },
}
impl EffectTarget {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Workspace => true,
            Self::WorkerView {
                assignment_id,
                base_digest,
                revision,
                cache_generation,
            } => {
                assignment_id.valid()
                    && *base_digest != [0; 32]
                    && *revision <= 16
                    && *cache_generation != Some([0; 32])
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDataPurpose {
    Base,
    Mutation,
    Handover,
}
impl WorkerDataPurpose {
    pub(crate) fn limit(self) -> usize {
        if self == Self::Base {
            WORKER_MANIFEST_BYTES
        } else {
            WORKER_DATA_BYTES
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDataRef {
    pub assignment_id: AgentRunId,
    pub purpose: WorkerDataPurpose,
    pub first_sequence: u64,
    pub seal_sequence: u64,
    pub chunks: u16,
    pub bytes: u32,
    pub sha256: [u8; 32],
}
impl WorkerDataRef {
    pub(crate) fn valid(&self) -> bool {
        self.assignment_id.valid()
            && self.first_sequence > 0
            && self.chunks > 0
            && usize::from(self.chunks) <= self.purpose.limit().div_ceil(WORKER_CHUNK_BYTES)
            && self
                .first_sequence
                .checked_add(u64::from(self.chunks))
                .is_some_and(|minimum| minimum <= self.seal_sequence)
            && self.bytes > 0
            && self.bytes as usize <= self.purpose.limit()
            && (self.bytes as usize).div_ceil(WORKER_CHUNK_BYTES) == usize::from(self.chunks)
            && self.sha256 != [0; 32]
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDataChunk {
    pub assignment_id: AgentRunId,
    pub purpose: WorkerDataPurpose,
    pub tool_id: uuid::Uuid,
    pub ordinal: u16,
    pub bytes: u32,
    pub sha256: [u8; 32],
    pub content: String,
}
impl WorkerDataChunk {
    pub(crate) fn decoded(&self) -> Option<Vec<u8>> {
        if !self.assignment_id.valid()
            || self.tool_id.get_version() != Some(uuid::Version::SortRand)
            || self.bytes == 0
            || self.bytes as usize > WORKER_CHUNK_BYTES
            || usize::from(self.ordinal) >= self.purpose.limit().div_ceil(WORKER_CHUNK_BYTES)
            || self.content.len() > WORKER_CHUNK_BYTES.div_ceil(3) * 4
        {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.content)
            .ok()?;
        (bytes.len() == self.bytes as usize
            && crate::tools::types::digest(&bytes) == self.sha256
            && base64::engine::general_purpose::STANDARD.encode(&bytes) == self.content)
            .then_some(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDataSealed {
    pub tool_id: uuid::Uuid,
    pub data: WorkerDataRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHandover {
    pub criterion_evidence: Vec<u64>,
    pub assignment_id: AgentRunId,
    pub base_digest: [u8; 32],
    pub data: WorkerDataRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStopReason {
    Cancelled,
    Paused,
    Budget,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStopRecord {
    pub reason: WorkerStopReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDisposition {
    Ready,
    NoChange,
    Failed,
    Cancelled,
    Lost,
    PrivateUncertain,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerCause {
    Finished,
    StoppedByHook,
    InvalidOutcome,
    ToolFailed,
    TurnLimit,
    Cancelled,
    Budget,
    SnapshotDrift,
    CriterionUnverified,
    PrivateUncertain,
    Lost,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOutcome {
    #[serde(deserialize_with = "Option::deserialize")]
    pub cleanup: Option<WorkerCleanupReceipt>,
    pub disposition: WorkerDisposition,
    pub cause: WorkerCause,
    #[serde(deserialize_with = "Option::deserialize")]
    pub data: Option<WorkerDataRef>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerIntegrationOutcome {
    Integrated,
    Conflict,
    Denied,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPublication {
    Unpublished,
    Confirmed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkerHistory {
    pub run_id: super::RunId,
    pub assignment_id: AgentRunId,
    pub resumes: Option<AgentRunId>,
    pub supersedes: Option<AgentRunId>,
    pub status: String,
    pub cause: Option<WorkerCause>,
    pub publication: WorkerPublication,
    pub integration: Option<WorkerIntegrationOutcome>,
    pub discarded: Option<WorkerSettlementReason>,
    pub private_outcome_unknown: bool,
    pub cleanup_confirmed: bool,
    pub lease_released: bool,
    pub verified: bool,
    pub report: String,
}

impl WorkerHistory {
    pub fn description(&self) -> String {
        format!(
            "Worker {}: {}; cause={:?}; publication={:?}; integration={:?}; discard={:?}; private_outcome_unknown={}; cleanup_confirmed={}; lease_released={}; verified={}; resumes={}; supersedes={}",
            self.assignment_id,
            self.status,
            self.cause,
            self.publication,
            self.integration,
            self.discarded,
            self.private_outcome_unknown,
            self.cleanup_confirmed,
            self.lease_released,
            self.verified,
            self.resumes
                .map_or_else(|| "none".into(), |id| id.to_string()),
            self.supersedes
                .map_or_else(|| "none".into(), |id| id.to_string()),
        )
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIntegration {
    pub assignment_id: AgentRunId,
    pub handover_sequence: u64,
    pub tool_id: uuid::Uuid,
    pub finished_sequence: u64,
    pub outcome: WorkerIntegrationOutcome,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSettlementReason {
    Integrated,
    Failed,
    Cancelled,
    Stopped,
    Conflict,
    Denied,
    NoChange,
    PrivateUncertain,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSettlement {
    pub assignment_id: AgentRunId,
    #[serde(deserialize_with = "Option::deserialize")]
    pub handover_sequence: Option<u64>,
    pub reason: WorkerSettlementReason,
    pub cause_sequence: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerVerification {
    pub assignment_id: AgentRunId,
    pub criterion_digest: [u8; 32],
    pub passed: bool,
    pub evidence: Vec<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangesetSource {
    Rewind {
        plan_digest: [u8; 32],
        bytes: u32,
        sha256: [u8; 32],
    },
    CommandReceipt {
        source_intent_digest: [u8; 32],
        changeset: crate::tools::CommandChangeset,
    },
    WorkerHandover {
        assignment_id: AgentRunId,
        handover_sequence: u64,
        data: WorkerDataRef,
    },
}
impl ChangesetSource {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Rewind {
                plan_digest,
                bytes,
                sha256,
            } => {
                *plan_digest != [0; 32]
                    && *sha256 != [0; 32]
                    && *bytes > 0
                    && (*bytes as usize) <= WORKER_DATA_BYTES
            }
            Self::CommandReceipt {
                source_intent_digest,
                changeset,
            } => {
                *source_intent_digest != [0; 32] && !changeset.files.is_empty() && changeset.valid()
            }
            Self::WorkerHandover {
                assignment_id,
                handover_sequence,
                data,
            } => {
                assignment_id.valid()
                    && *assignment_id == data.assignment_id
                    && *handover_sequence > data.seal_sequence
                    && matches!(
                        data.purpose,
                        WorkerDataPurpose::Handover | WorkerDataPurpose::Mutation
                    )
                    && data.valid()
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPreparation {
    pub assignment_id: AgentRunId,
    pub purpose: WorkerDataPurpose,
    pub max_chunks: u16,
    pub max_bytes: u32,
}
impl WorkerPreparation {
    pub(crate) fn valid(&self) -> bool {
        self.assignment_id.valid()
            && self.purpose != WorkerDataPurpose::Mutation
            && self.max_bytes > 0
            && self.max_bytes as usize <= self.purpose.limit()
            && usize::from(self.max_chunks)
                == (self.max_bytes as usize).div_ceil(WORKER_CHUNK_BYTES)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerMutation {
    pub previous_revision: u32,
    pub next_revision: u32,
    pub data: WorkerDataRef,
}

pub(crate) fn writable_path(path: &str) -> bool {
    path.is_ascii()
        && crate::tools::types::valid_relative(path, false)
        && path.split('/').count() <= 16
}
pub(crate) fn contains(root: &str, path: &str) -> bool {
    root == "." || root == path || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}
pub(crate) fn within(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|r| contains(r, path))
}
pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    contains(&a, &b) || contains(&b, &a)
}
pub(crate) fn valid_leases(roots: &[String]) -> bool {
    !roots.is_empty()
        && roots.len() <= 64
        && roots
            .iter()
            .enumerate()
            .all(|(i, p)| writable_path(p) && roots[..i].iter().all(|r| r < p && !overlaps(r, p)))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerManifest {
    #[serde(deserialize_with = "bounded_files")]
    pub entries: Vec<WorkerManifestEntry>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerManifestEntry {
    pub path: String,
    pub version: WorkerBaseVersion,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerBaseVersion {
    Absent,
    Directory,
    File {
        sha256: String,
        bytes: u32,
        executable: bool,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerChangeset {
    #[serde(deserialize_with = "bounded_files")]
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFileFact {
    pub path: String,
    pub before: WorkerBaseVersion,
    pub after: WorkerBaseVersion,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerDataFacts {
    Manifest(Vec<WorkerManifestEntry>),
    Changes(Vec<WorkerFileFact>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedVersion<'a> {
    #[serde(borrow)]
    sha256: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    content: std::borrow::Cow<'a, str>,
    executable: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedChange<'a> {
    #[serde(borrow)]
    path: std::borrow::Cow<'a, str>,
    #[serde(borrow, deserialize_with = "Option::deserialize")]
    before: Option<BorrowedVersion<'a>>,
    #[serde(borrow, deserialize_with = "Option::deserialize")]
    after: Option<BorrowedVersion<'a>>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedChangeset<'a> {
    #[serde(borrow, deserialize_with = "bounded_files")]
    files: Vec<BorrowedChange<'a>>,
}

pub(crate) fn decode_data(
    bytes: &[u8],
    purpose: WorkerDataPurpose,
) -> Result<WorkerDataFacts, ReplayError> {
    decode_data_inner(bytes, purpose, WORKER_CONTENT_BYTES, false)
}

pub(crate) fn decode_rewind_data(bytes: &[u8]) -> Result<WorkerDataFacts, ReplayError> {
    let facts = decode_data_inner(
        bytes,
        WorkerDataPurpose::Handover,
        2 * WORKER_CONTENT_BYTES,
        true,
    )?;
    if let WorkerDataFacts::Changes(files) = &facts {
        for before in [true, false] {
            let size: usize = files
                .iter()
                .map(
                    |file| match if before { &file.before } else { &file.after } {
                        WorkerBaseVersion::File { bytes, .. } => *bytes as usize,
                        _ => 0,
                    },
                )
                .sum();
            if size > WORKER_CONTENT_BYTES {
                return Err(ReplayError::InvalidPayload);
            }
        }
    }
    Ok(facts)
}

fn decode_data_inner(
    bytes: &[u8],
    purpose: WorkerDataPurpose,
    content_limit: usize,
    rewind: bool,
) -> Result<WorkerDataFacts, ReplayError> {
    if bytes.is_empty() || bytes.len() > purpose.limit() {
        return Err(ReplayError::InvalidPayload);
    }
    if purpose == WorkerDataPurpose::Base {
        let manifest: WorkerManifest =
            serde_json::from_slice(bytes).map_err(|_| ReplayError::InvalidPayload)?;
        if manifest.entries.len() > WORKER_FILES || !canonical(&manifest, bytes) {
            return Err(ReplayError::InvalidPayload);
        }
        let mut total = 0usize;
        for (i, entry) in manifest.entries.iter().enumerate() {
            if !crate::tools::types::valid_relative(&entry.path, false)
                || entry.path.split('/').count() > 16
                || manifest.entries[..i].iter().any(|e| {
                    e.path >= entry.path
                        || contains(&e.path, &entry.path)
                            && e.version != WorkerBaseVersion::Directory
                })
            {
                return Err(ReplayError::InvalidPayload);
            }
            if let WorkerBaseVersion::File { sha256, bytes, .. } = &entry.version {
                if !crate::tools::types::valid_digest(sha256) || *bytes as usize > 1024 * 1024 {
                    return Err(ReplayError::InvalidPayload);
                }
                total = total
                    .checked_add(*bytes as usize)
                    .ok_or(ReplayError::InvalidPayload)?;
            }
        }
        if total > WORKER_CONTENT_BYTES {
            return Err(ReplayError::InvalidPayload);
        }
        return Ok(WorkerDataFacts::Manifest(manifest.entries));
    }
    let changeset: BorrowedChangeset<'_> =
        serde_json::from_slice(bytes).map_err(|_| ReplayError::InvalidPayload)?;
    if changeset.files.len() > WORKER_FILES || !canonical(&changeset, bytes) {
        return Err(ReplayError::InvalidPayload);
    }
    let mut total = 0usize;
    let mut facts = Vec::with_capacity(changeset.files.len());
    let lowered: Vec<String> = changeset
        .files
        .iter()
        .map(|file| file.path.to_ascii_lowercase())
        .collect();
    for (i, file) in changeset.files.iter().enumerate() {
        if !writable_path(&file.path)
            || !rewind && matches!((&file.before, &file.after), (None, None))
            || !rewind
                && matches!((&file.before, &file.after), (Some(a),Some(b)) if a.sha256 == b.sha256 && a.executable == b.executable)
            || changeset.files[..i]
                .iter()
                .zip(&lowered)
                .any(|(f, earlier)| {
                    f.path >= file.path
                        || contains(earlier, &lowered[i])
                        || contains(&lowered[i], earlier)
                })
        {
            return Err(ReplayError::InvalidPayload);
        }
        let mut version =
            |v: &Option<BorrowedVersion<'_>>| -> Result<WorkerBaseVersion, ReplayError> {
                let Some(v) = v else {
                    return Ok(WorkerBaseVersion::Absent);
                };
                if !crate::tools::types::valid_digest(&v.sha256)
                    || v.content.len() > (1024usize * 1024).div_ceil(3) * 4
                {
                    return Err(ReplayError::InvalidPayload);
                }
                let content = base64::engine::general_purpose::STANDARD
                    .decode(v.content.as_bytes())
                    .map_err(|_| ReplayError::InvalidPayload)?;
                if content.len() > 1024 * 1024
                    || base64::engine::general_purpose::STANDARD.encode(&content) != v.content
                    || crate::tools::types::hex_digest(&content) != v.sha256
                {
                    return Err(ReplayError::InvalidPayload);
                }
                total = total
                    .checked_add(content.len())
                    .ok_or(ReplayError::InvalidPayload)?;
                if total > content_limit {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(WorkerBaseVersion::File {
                    sha256: v.sha256.to_string(),
                    bytes: u32::try_from(content.len()).map_err(|_| ReplayError::InvalidPayload)?,
                    executable: v.executable,
                })
            };
        facts.push(WorkerFileFact {
            path: file.path.to_string(),
            before: version(&file.before)?,
            after: version(&file.after)?,
        });
    }
    Ok(WorkerDataFacts::Changes(facts))
}

fn canonical(value: &impl Serialize, bytes: &[u8]) -> bool {
    struct Compare<'a>(&'a [u8]);
    impl std::io::Write for Compare<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.0.starts_with(buf) {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            self.0 = &self.0[buf.len()..];
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut compare = Compare(bytes);
    serde_json::to_writer(&mut compare, value).is_ok() && compare.0.is_empty()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCleanupReceipt {
    pub assignment_id: AgentRunId,
    pub base_digest: [u8; 32],
    pub revision: u32,
    pub enforcement_digest: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    pub last_intent_digest: Option<[u8; 32]>,
}

fn bounded_files<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Files<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Files<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 2048 worker file records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut files = Vec::new();
            while let Some(file) = seq.next_element()? {
                if files.len() == WORKER_FILES {
                    return Err(serde::de::Error::custom("worker file bound exceeded"));
                }
                files.push(file);
            }
            Ok(files)
        }
    }
    deserializer.deserialize_seq(Files(std::marker::PhantomData))
}

impl WorkerOutcome {
    pub(crate) fn valid(&self) -> bool {
        matches!(
            (self.disposition, self.cause),
            (
                WorkerDisposition::Ready | WorkerDisposition::NoChange,
                WorkerCause::Finished
            ) | (
                WorkerDisposition::Failed,
                WorkerCause::StoppedByHook
                    | WorkerCause::InvalidOutcome
                    | WorkerCause::ToolFailed
                    | WorkerCause::TurnLimit
                    | WorkerCause::Budget
                    | WorkerCause::SnapshotDrift
                    | WorkerCause::CriterionUnverified
            ) | (WorkerDisposition::Cancelled, WorkerCause::Cancelled)
                | (WorkerDisposition::Lost, WorkerCause::Lost)
                | (
                    WorkerDisposition::PrivateUncertain,
                    WorkerCause::PrivateUncertain
                )
        ) && matches!(
            self.disposition,
            WorkerDisposition::Ready | WorkerDisposition::NoChange
        ) == self.data.is_some()
            && self.data.as_ref().is_none_or(WorkerDataRef::valid)
            && self.cleanup.as_ref().is_none_or(|c| {
                c.assignment_id.valid()
                    && c.base_digest != [0; 32]
                    && c.revision <= 16
                    && c.enforcement_digest != [0; 32]
                    && c.last_intent_digest != Some([0; 32])
            })
    }
}

pub(crate) fn valid_commands(commands: &[String]) -> bool {
    commands.len() <= 8
        && commands.iter().enumerate().all(|(i, name)| {
            crate::tools::types::valid_name(name)
                && commands[..i].iter().all(|earlier| earlier < name)
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrivateCapture {
    None {},
    Sealed { data: WorkerDataRef },
    Rejected { cause: PrivateCaptureRejection },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateCaptureRejection {
    OutOfLease,
    Overflow,
    UnsupportedEntry,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCommandResult {
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub private_capture: PrivateCapture,
}

impl WorkerCommandResult {
    pub(crate) fn valid(&self) -> bool {
        serde_json::to_vec(self)
            .is_ok_and(|b| b.len() <= crate::tools::types::MAX_TOOL_RESULT_BYTES)
            && match &self.private_capture {
                PrivateCapture::Sealed { data } => {
                    data.valid() && data.purpose == WorkerDataPurpose::Mutation
                }
                _ => true,
            }
    }
}

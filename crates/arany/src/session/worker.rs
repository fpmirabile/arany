pub use super::EffectTarget;
use super::{AgentRunId, GoalSuccessCriterion, ReplayError};
use crate::tools::{FileChange, ToolCall};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

mod report;
pub(crate) use report::changed_paths;
pub const MAX_EDITOR_CHANGE_REPORT_BYTES: usize = 2048;

pub const WORKER_CHUNK_BYTES: usize = 45 * 1024;
pub const WORKER_DATA_BYTES: usize = 48 * 1024 * 1024;
pub const WORKER_CONTENT_BYTES: usize = 32 * 1024 * 1024;
pub const WORKER_FILES: usize = 2048;
pub(crate) const MAX_EDITOR_CONTINUATIONS: u8 = 3;
pub(crate) const MAX_WORKER_EVIDENCE: usize = 8;
const MAX_WORKER_TOOLS: usize = 15;

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
    Glob,
    Mkdir,
    Write,
    Edit,
    #[serde(rename = "multiedit")]
    MultiEdit,
    Delete,
    Move,
    Skill,
    McpList,
    Command,
    WebFetch,
    WebSearch,
}

impl WorkerTool {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
            Self::Search => "search",
            Self::Glob => "glob",
            Self::Mkdir => "mkdir",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::MultiEdit => "multiedit",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Skill => "skill",
            Self::McpList => "mcp_list",
            Self::Command => "command",
            Self::WebFetch => "web_fetch",
            Self::WebSearch => "web_search",
        }
    }

    pub(crate) fn of(call: &ToolCall) -> Option<Self> {
        Some(match call {
            ToolCall::List { .. } => Self::List,
            ToolCall::Read { .. } => Self::Read,
            ToolCall::Search { .. } => Self::Search,
            ToolCall::Glob { .. } => Self::Glob,
            ToolCall::Mkdir { .. } => Self::Mkdir,
            ToolCall::Write { .. } => Self::Write,
            ToolCall::Edit { .. } => Self::Edit,
            ToolCall::MultiEdit { .. } => Self::MultiEdit,
            ToolCall::Delete { .. } => Self::Delete,
            ToolCall::Move { .. } => Self::Move,
            ToolCall::Skill { .. } => Self::Skill,
            ToolCall::McpList { .. } => Self::McpList,
            ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. } => Self::Command,
            ToolCall::WebFetch { .. } => Self::WebFetch,
            ToolCall::WebSearch { .. } => Self::WebSearch,
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
                .all(|p| crate::validate::valid_relative(p, true))
            && (self.write_paths == ["."] || valid_leases(&self.write_paths))
            && !self.tools.is_empty()
            && self.tools.len() <= MAX_WORKER_TOOLS
            && self
                .tools
                .iter()
                .enumerate()
                .all(|(i, t)| !self.tools[..i].contains(t))
            && (!self.commands.is_empty()
                || self.tools.iter().any(|t| {
                    matches!(
                        t,
                        WorkerTool::Write
                            | WorkerTool::Edit
                            | WorkerTool::MultiEdit
                            | WorkerTool::Delete
                            | WorkerTool::Move
                    )
                }))
            && self.write_paths.iter().all(|p| within(p, &self.read_paths))
            && [&self.skills, &self.mcp_servers].iter().all(|names| {
                names.len() <= 64
                    && names
                        .iter()
                        .enumerate()
                        .all(|(i, n)| crate::validate::valid_name(n) && !names[..i].contains(n))
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

    pub(crate) fn narrowed_for_editor(
        mut self,
        request: &EditorAssignment,
        plan_commands: &[String],
        allows_tool: impl Fn(WorkerTool) -> bool,
    ) -> Self {
        self.read_paths = request.lease.clone();
        self.write_paths = request.lease.clone();
        self.tools.retain(|tool| {
            !matches!(
                tool,
                WorkerTool::Skill
                    | WorkerTool::McpList
                    | WorkerTool::WebFetch
                    | WorkerTool::WebSearch
            ) && allows_tool(*tool)
        });
        self.commands.retain(|c| {
            request.commands.contains(c)
                && plan_commands.contains(c)
                && allows_tool(WorkerTool::Command)
        });
        self.skills.clear();
        self.mcp_servers.clear();
        self
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        WorkerTool::of(call).is_some_and(|t| self.tools.contains(&t))
            && match call {
                ToolCall::List { path }
                | ToolCall::Read { path, .. }
                | ToolCall::Search { path, .. }
                | ToolCall::Glob { path, .. } => within(path, &self.read_paths),
                ToolCall::Mkdir { path }
                | ToolCall::Write { path, .. }
                | ToolCall::Edit { path, .. }
                | ToolCall::Delete { path, .. }
                | ToolCall::MultiEdit {
                    file_path: path, ..
                } => writable_path(path) && within(path, &self.write_paths),
                ToolCall::Move {
                    source,
                    destination,
                    ..
                } => [source, destination]
                    .iter()
                    .all(|p| writable_path(p) && within(p, &self.write_paths)),
                ToolCall::Skill { name, .. } => self.skills.contains(name),
                ToolCall::McpList { server } => self.mcp_servers.contains(server),
                ToolCall::Command { program, cwd, .. } => {
                    self.commands.contains(program) && (cwd == "." || within(cwd, &self.read_paths))
                }
                ToolCall::CommandUnguarded { cwd, .. } => {
                    cwd.is_empty() || cwd == "." || within(cwd, &self.read_paths)
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
}
impl WorkerPolicy {
    pub(crate) fn valid(&self) -> bool {
        self.parent_grant.valid()
            && (1..=8).contains(&self.concurrency)
            && (1..=17).contains(&self.max_turns)
            && self.max_tools <= 16
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
                        .is_none_or(|n| crate::validate::valid_name(n))
            }
            Self::Editor { assignment: a } => {
                crate::validate::valid_name(&a.definition)
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
                crate::validate::valid_relative(path, false)
                    && crate::validate::valid_digest(sha256)
            }
            GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                crate::validate::valid_relative(path, false) && content.len() <= 8 * 1024
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
}
#[must_use]
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
    ChangesTooLarge,
    IntegrationConflict,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOutcome {
    pub disposition: WorkerDisposition,
    pub cause: WorkerCause,
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
    pub lease_released: bool,
    pub verified: bool,
    pub report: String,
    pub changed_paths: String,
}

impl WorkerHistory {
    pub fn description(&self) -> String {
        format!(
            "Worker {}: {}; cause={:?}; publication={:?}; lease_released={}; verified={}; resumes={}; supersedes={}",
            self.assignment_id,
            self.status,
            self.cause,
            self.publication,
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
pub struct WorkerLeaseRelease {
    pub assignment_id: AgentRunId,
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
    // An editor's one integration of its workspace; content stays in that workspace until publication.
    EditorWorkspace {
        files: Vec<EditorChange>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorChange {
    pub path: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub before: Option<super::rewind::FileState>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub after: Option<super::rewind::FileState>,
}

// The user's "Editor changes" settings bound one integration; the Goal pins them at admission.
pub const DEFAULT_EDITOR_CHANGE_FILES: u32 = 512;
pub const DEFAULT_EDITOR_CHANGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EDITOR_CHANGE_FILES: u32 = WORKER_FILES as u32;
pub const MAX_EDITOR_CHANGE_BYTES: u64 = 1024 * 1024 * 1024;

pub(crate) fn valid_editor_changes(files: &[EditorChange]) -> bool {
    !files.is_empty()
        && files.len() <= WORKER_FILES
        && files.iter().enumerate().all(|(index, change)| {
            writable_path(&change.path)
                && change.before != change.after
                && change
                    .before
                    .iter()
                    .chain(&change.after)
                    .all(|state| state.valid())
                && files[..index].iter().all(|earlier| {
                    earlier.path < change.path
                        && !std::path::Path::new(&change.path).starts_with(&earlier.path)
                })
        })
        && serde_json::to_vec(files)
            .is_ok_and(|bytes| bytes.len() <= crate::tools::types::MAX_TOOL_ARGUMENT_BYTES)
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
            Self::EditorWorkspace { files } => valid_editor_changes(files),
        }
    }
}

pub(crate) fn writable_path(path: &str) -> bool {
    path.is_ascii() && crate::validate::valid_relative(path, false) && path.split('/').count() <= 16
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

pub(crate) fn decode_rewind_data(bytes: &[u8]) -> Result<Vec<WorkerFileFact>, ReplayError> {
    if bytes.is_empty() || bytes.len() > WORKER_DATA_BYTES {
        return Err(ReplayError::InvalidPayload);
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
        facts.push(WorkerFileFact {
            path: file.path.to_string(),
            before: rewind_version(&file.before, &mut total)?,
            after: rewind_version(&file.after, &mut total)?,
        });
    }
    for before in [true, false] {
        let size: usize = facts
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
    Ok(facts)
}

fn rewind_version(
    v: &Option<BorrowedVersion<'_>>,
    total: &mut usize,
) -> Result<WorkerBaseVersion, ReplayError> {
    let Some(v) = v else {
        return Ok(WorkerBaseVersion::Absent);
    };
    if !crate::validate::valid_digest(&v.sha256)
        || v.content.len() > crate::tools::types::MAX_FILE_BYTES.div_ceil(3) * 4
    {
        return Err(ReplayError::InvalidPayload);
    }
    let content = base64::engine::general_purpose::STANDARD
        .decode(v.content.as_bytes())
        .map_err(|_| ReplayError::InvalidPayload)?;
    if content.len() > crate::tools::types::MAX_FILE_BYTES
        || base64::engine::general_purpose::STANDARD.encode(&content) != v.content
        || crate::validate::hex_digest(&content) != v.sha256
    {
        return Err(ReplayError::InvalidPayload);
    }
    *total = total
        .checked_add(content.len())
        .ok_or(ReplayError::InvalidPayload)?;
    if *total > 2 * WORKER_CONTENT_BYTES {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(WorkerBaseVersion::File {
        sha256: v.sha256.to_string(),
        bytes: u32::try_from(content.len()).map_err(|_| ReplayError::InvalidPayload)?,
        executable: v.executable,
    })
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

fn bounded_files<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    crate::validate::bounded_sequence::<D, T, T, WORKER_FILES>(
        deserializer,
        format_args!("at most {WORKER_FILES} worker file records"),
        "worker file bound exceeded",
        |_| Ok(()),
    )
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
                    | WorkerCause::ChangesTooLarge
                    | WorkerCause::IntegrationConflict
            ) | (WorkerDisposition::Cancelled, WorkerCause::Cancelled)
        )
    }
}

pub(crate) fn valid_commands(commands: &[String]) -> bool {
    commands.len() <= 8
        && commands.iter().enumerate().all(|(i, name)| {
            crate::validate::valid_name(name) && commands[..i].iter().all(|earlier| earlier < name)
        })
}

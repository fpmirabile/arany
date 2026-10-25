mod goal;
mod records;
#[cfg(test)]
mod tests;
mod validate;
use super::worker::*;

use super::{
    MAX_ASSISTANT_MESSAGE_BYTES, MAX_COMPACTION_OUTPUT_TOKENS, MAX_COMPACTION_SUMMARY_BYTES,
    MAX_HISTORY_RUNS, MAX_STEERING_NOTE_BYTES, MAX_USER_MESSAGE_BYTES, MAX_WINDOW_CONTENT_BYTES,
    compaction, content_ceiling,
};

pub use goal::{
    BudgetAction, BudgetChargeBasis, BudgetRefusal, BudgetRefusalCause, GoalBudget, GoalMcpTool,
    GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus, GoalSuccessCriterion,
    MAX_BUDGET_REFUSAL_BYTES, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_CONTRACT_BYTES,
    MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES,
    MAX_GOAL_RUNS,
};
pub(crate) use goal::{GoalSuccessState, GoalUsage, STORE_ACTION_OVERHEAD_MS};
pub use records::{
    AgentDisposition, AgentRole, ChildGrant, CollaborationPolicy, CompactionFailure,
    CompactionRecord, CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory,
    DisplayChunk, InstructionFile, InstructionSource, MAX_DISPLAY_CHUNK_BYTES, MAX_DISPLAY_CHUNKS,
    MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES, ProposalRefusal, ProposalRefusalCause,
    ProviderCallDisposition, ProviderCallRecord, RunConfig, RunDisposition, RunFailure,
    SessionDefaults,
};
use validate::bounded;

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::{Uuid, Version};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceIdentity {
    pub device: u64,
    pub inode: u64,
}

fn parse_v7(value: &str) -> Result<Uuid, &'static str> {
    let id = Uuid::parse_str(value).map_err(|_| "invalid ID")?;
    if id.get_version() != Some(Version::SortRand) {
        return Err("ID must be UUIDv7");
    }
    Ok(id)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for SessionId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(Uuid);

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for RunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GoalId(Uuid);

impl GoalId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    fn valid(self) -> bool {
        self.0.get_version() == Some(Version::SortRand)
    }
}
impl Default for GoalId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for GoalId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

pub(crate) const PAYLOAD_VERSIONS: [(&str, i64); 44] = [
    ("SessionStarted", 6),
    ("SessionDefaultChanged", 6),
    ("SessionForked", 6),
    ("SessionRenamed", 6),
    ("ProviderSetupEditStarted", 1),
    ("ProviderSetupEditFinished", 1),
    ("GoalRunLinked", 6),
    ("GoalResumed", 6),
    ("GoalPlanContractRecorded", 10),
    ("GoalContextCompacted", 6),
    ("GoalSteeringNoteRecorded", 6),
    ("AgentSteeringNoteRecorded", 1),
    ("AssistantDisplayRecorded", 6),
    ("MessageCommitted", 6),
    ("MessageAccepted", 7),
    ("AgentSpawned", 14),
    ("AgentFinished", 10),
    ("ProviderCallRecorded", 20),
    ("WorkerDataChunk", 1),
    ("WorkerDataSealed", 1),
    ("WorkerStopObserved", 1),
    ("WorkerIntegrationRecorded", 1),
    ("WorkerHandoverDiscarded", 1),
    ("WorkerLeaseReleased", 1),
    ("WorkerVerificationRecorded", 1),
    ("WorkerHandoverRecorded", 2),
    ("ToolResultsCleared", 1),
    ("GoalStarted", 8),
    ("GoalFinished", 8),
    ("SupervisorVerdictRecorded", 1),
    ("WorkspaceRewindStarted", 2),
    ("WorkspaceRewindFinished", 1),
    ("ProviderFallbackUsed", 1),
    ("ProviderRetryScheduled", 1),
    ("ToolPermissionDecided", 6),
    ("HookStarted", 9),
    ("HookFinished", 15),
    ("ToolFinished", 26),
    ("RunStarted", 22),
    ("RunFinished", 9),
    ("GoalProviderCallReserved", 7),
    ("GoalCompactionCallReserved", 7),
    ("ToolStarted", 19),
    ("ContextCompacted", 7),
];

pub(crate) fn payload_version(kind: &str) -> Option<i64> {
    PAYLOAD_VERSIONS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, version)| *version)
}
pub(crate) const MAX_CHILD_TOOL_CALLS: usize = 16;
// Every model Tool call takes one inference turn, plus the final report.
pub(crate) const MAX_CHILD_TURNS: u8 = {
    assert!(MAX_CHILD_TOOL_CALLS < u8::MAX as usize);
    MAX_CHILD_TOOL_CALLS as u8 + 1
};
pub(crate) const MAX_STEP_TOOL_CALLS: u8 = 4;
const _: () = assert!(MAX_STEP_TOOL_CALLS as u32 <= u8::MAX as u32);
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentRunId(Uuid);

impl AgentRunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn valid(self) -> bool {
        self.0.get_version() == Some(Version::SortRand)
    }
}
impl Default for AgentRunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for AgentRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for AgentRunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    WorkspaceRewindStarted {
        record: Box<super::rewind::RewindStarted>,
    },
    WorkspaceRewindFinished {
        record: Box<super::rewind::RewindFinished>,
    },
    SessionStarted {
        title: String,
        workspace_identity: WorkspaceIdentity,
    },
    SessionDefaultChanged {
        defaults: SessionDefaults,
    },
    SessionForked {
        title: String,
        source_session_id: SessionId,
        source_run_id: RunId,
        source_sequence: u64,
        prefix_digest: [u8; 32],
    },
    ContextCompacted {
        record: CompactionRecord,
        /// The Goal compaction call's settled active time; `None` outside a Goal.
        active_ms: Option<u64>,
    },
    SessionRenamed {
        title: String,
    },
    ProviderSetupEditStarted {
        operation_id: uuid::Uuid,
        target: ProviderSetupTarget,
        expected_digest: Option<[u8; 32]>,
    },
    ProviderSetupEditFinished {
        operation_id: uuid::Uuid,
        resulting_digest: Option<[u8; 32]>,
        disposition: ProviderSetupDisposition,
    },
    GoalStarted {
        goal_id: GoalId,
        objective: String,
        include_paths: Option<Vec<String>>,
        workspace_identity: WorkspaceIdentity,
        budget: GoalBudget,
        phase: GoalPhase,
        /// Engine iterates Runs on the pinned objective until a Goal stop condition.
        autonomous: bool,
        resource_settings: crate::tools::ToolResourceSettings,
        /// The exhausted Goal this one continues with a fresh budget; it stays closed.
        continues: Option<GoalId>,
    },
    GoalRunLinked {
        goal_id: GoalId,
        run_id: RunId,
        phase: GoalPhase,
    },
    GoalResumed {
        goal_id: GoalId,
    },
    GoalPlanContractRecorded {
        goal_id: GoalId,
        run_id: RunId,
        contract: GoalPlanContract,
    },
    GoalFinished {
        goal_id: GoalId,
        status: GoalStatus,
        active_ms: u64,
        report: GoalReport,
        /// A Failed Goal closed because its budget ran out, not because a Run failed for another reason.
        exhausted: bool,
        budget_refusal: Option<BudgetRefusal>,
    },
    MessageAccepted {
        skill: Option<String>,
        run_id: RunId,
        text: String,
        images: Vec<crate::provider::ImageAttachment>,
    },
    RunStarted {
        run_id: RunId,
        config: RunConfig,
        /// Active time the Run may spend: the Goal's remaining time at start; `None` when unlimited.
        active_deadline_ms: Option<u64>,
    },
    AgentSpawned {
        run_id: RunId,
        agent_run_id: AgentRunId,
        role: AgentRole,
        ordinal: u8,
        objective: Option<String>,
        grant: Option<ChildGrant>,
        definition: Option<String>,
        profile: Option<String>,
        assignment: Option<WorkerAssignment>,
    },
    ProviderFallbackUsed {
        run_id: RunId,
        agent_run_id: AgentRunId,
        fallback: crate::provider::ProviderFallback,
    },
    ProviderRetryScheduled {
        run_id: RunId,
        agent_run_id: AgentRunId,
        retry: crate::provider::ProviderRetry,
    },
    ProviderCallRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: ProviderCallRecord,
    },
    GoalProviderCallReserved {
        goal_id: GoalId,
        run_id: RunId,
        agent_run_id: AgentRunId,
        /// Upper bound of the Run's active time until the next action starts; a crash charges it.
        active_ceiling_ms: u64,
    },
    GoalCompactionCallReserved {
        goal_id: GoalId,
        run_id: RunId,
        active_ceiling_ms: u64,
    },
    ToolResultsCleared {
        run_id: RunId,
        covered_tool_count: u32,
        cleared_bytes: u32,
    },
    GoalContextCompacted {
        goal_id: GoalId,
        run_id: RunId,
        covered_tool_count: u32,
        record: CompactionRecord,
    },
    GoalSteeringNoteRecorded {
        goal_id: GoalId,
        run_id: RunId,
        text: String,
    },
    AgentSteeringNoteRecorded {
        goal_id: GoalId,
        run_id: RunId,
        agent_run_id: AgentRunId,
        text: String,
    },
    AssistantDisplayRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        chunk: DisplayChunk,
    },
    ToolStarted {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: crate::tools::EffectIntent,
        active_ceiling_ms: u64,
    },
    SupervisorVerdictRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        verdict: crate::supervisor::SupervisorVerdict,
    },
    ToolFinished {
        checkpoint: Option<super::Checkpoint>,
        run_id: RunId,
        agent_run_id: AgentRunId,
        observation: crate::tools::ToolObservation,
    },
    ToolPermissionDecided {
        run_id: RunId,
        agent_run_id: AgentRunId,
        verdict: crate::tools::permissions::PermissionVerdict,
    },
    HookStarted {
        run_id: RunId,
        agent_run_id: AgentRunId,
        hook: crate::tools::permissions::HookInvocation,
    },
    HookFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        outcome: crate::tools::permissions::HookOutcome,
    },
    AgentFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        summary: Option<String>,
        result: Option<String>,
        worker: Option<WorkerOutcome>,
    },
    WorkerDataChunk {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerDataChunk,
    },
    WorkerDataSealed {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerDataSealed,
    },
    WorkerHandoverRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerHandover,
    },
    WorkerIntegrationRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerIntegration,
    },
    WorkerHandoverDiscarded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerSettlement,
    },
    WorkerLeaseReleased {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerSettlement,
    },
    WorkerVerificationRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerVerification,
    },
    WorkerStopObserved {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerStopRecord,
    },
    MessageCommitted {
        run_id: RunId,
        text: String,
    },
    RunFinished {
        run_id: RunId,
        disposition: RunDisposition,
        failure: Option<RunFailure>,
        active_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSetupTarget {
    pub kind: ProviderSetupTargetKind,
    pub identity: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderSetupTargetKind {
    NativeAccount,
    ChatGptAccount,
    CustomProfile,
    ModelPreferences,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderSetupDisposition {
    Applied,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    pub sequence: u64,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub agent_run_id: Option<AgentRunId>,
    pub event: Event,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForkLineage {
    pub source_session_id: SessionId,
    pub source_run_id: RunId,
    pub source_sequence: u64,
    pub prefix_digest: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("invalid Event payload")]
    InvalidPayload,
    #[error("unknown Event kind or version")]
    UnknownEvent,
    #[error("invalid Session transition")]
    InvalidTransition,
    #[error("invalid compaction snapshot")]
    InvalidSnapshot,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TitlePayload {
    title: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedPayload {
    title: String,
    workspace_device: u64,
    workspace_inode: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkPayload {
    title: String,
    source_session_id: SessionId,
    source_run_id: RunId,
    source_sequence: u64,
    prefix_digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    text: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    #[serde(deserialize_with = "required_skill")]
    skill: Option<String>,
    text: String,
    #[serde(deserialize_with = "crate::provider::deserialize_images")]
    images: Vec<crate::provider::ImageAttachment>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalStartedPayload {
    goal_id: GoalId,
    objective: String,
    #[serde(deserialize_with = "Option::deserialize")]
    include_paths: Option<Vec<String>>,
    workspace_device: u64,
    workspace_inode: u64,
    budget: GoalBudget,
    phase: GoalPhase,
    autonomous: bool,
    resource_settings: crate::tools::ToolResourceSettings,
    #[serde(deserialize_with = "Option::deserialize")]
    continues: Option<GoalId>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalRunPayload {
    goal_id: GoalId,
    phase: GoalPhase,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalIdPayload {
    goal_id: GoalId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalContractPayload {
    goal_id: GoalId,
    contract: GoalPlanContract,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalFinishedPayload {
    goal_id: GoalId,
    status: GoalStatus,
    active_ms: u64,
    report: GoalReport,
    exhausted: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    budget_refusal: Option<BudgetRefusal>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalProviderCallPayload {
    goal_id: GoalId,
    active_ceiling_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolResultsClearedPayload {
    covered_tool_count: u32,
    cleared_bytes: u32,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolFinishedPayload {
    #[serde(flatten)]
    observation: crate::tools::ToolObservation,
    #[serde(deserialize_with = "Option::deserialize")]
    checkpoint: Option<super::Checkpoint>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolStartedPayload {
    intent: crate::tools::EffectIntent,
    active_ceiling_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextCompactedPayload {
    record: CompactionRecord,
    #[serde(deserialize_with = "Option::deserialize")]
    active_ms: Option<u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalContextCompactionPayload {
    goal_id: GoalId,
    covered_tool_count: u32,
    record: CompactionRecord,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalSteeringNotePayload {
    goal_id: GoalId,
    text: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnPayload {
    #[serde(deserialize_with = "Option::deserialize")]
    profile: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    assignment: Option<WorkerAssignment>,
    #[serde(deserialize_with = "Option::deserialize")]
    definition: Option<String>,
    role: AgentRole,
    ordinal: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    objective: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    grant: Option<ChildGrant>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFinishedPayload {
    #[serde(deserialize_with = "Option::deserialize")]
    worker: Option<WorkerOutcome>,
    disposition: AgentDisposition,
    #[serde(deserialize_with = "Option::deserialize")]
    summary: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    result: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFinishedPayload {
    disposition: RunDisposition,
    #[serde(deserialize_with = "Option::deserialize")]
    failure: Option<RunFailure>,
    active_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunStartedPayload {
    config: RunConfig,
    #[serde(deserialize_with = "Option::deserialize")]
    active_deadline_ms: Option<u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSetupEditStartedPayload {
    operation_id: uuid::Uuid,
    target: ProviderSetupTarget,
    #[serde(deserialize_with = "Option::deserialize")]
    expected_digest: Option<[u8; 32]>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSetupEditFinishedPayload {
    operation_id: uuid::Uuid,
    #[serde(deserialize_with = "Option::deserialize")]
    resulting_digest: Option<[u8; 32]>,
    disposition: ProviderSetupDisposition,
}

impl Event {
    pub(crate) fn finished_payload_size(summary: &str, result: &str) -> Result<usize, ReplayError> {
        serialize(&AgentFinishedPayload {
            worker: None,
            disposition: AgentDisposition::Finished,
            summary: Some(summary.into()),
            result: Some(result.into()),
        })
        .map(|payload| payload.len())
    }

    pub fn version(&self) -> i64 {
        payload_version(self.kind()).expect("every Event kind has a payload version")
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::WorkspaceRewindStarted { .. } => "WorkspaceRewindStarted",
            Self::WorkspaceRewindFinished { .. } => "WorkspaceRewindFinished",
            Self::SessionStarted { .. } => "SessionStarted",
            Self::SessionDefaultChanged { .. } => "SessionDefaultChanged",
            Self::SessionForked { .. } => "SessionForked",
            Self::ContextCompacted { .. } => "ContextCompacted",
            Self::SessionRenamed { .. } => "SessionRenamed",
            Self::ProviderSetupEditStarted { .. } => "ProviderSetupEditStarted",
            Self::ProviderSetupEditFinished { .. } => "ProviderSetupEditFinished",
            Self::GoalStarted { .. } => "GoalStarted",
            Self::GoalRunLinked { .. } => "GoalRunLinked",
            Self::GoalResumed { .. } => "GoalResumed",
            Self::GoalPlanContractRecorded { .. } => "GoalPlanContractRecorded",
            Self::GoalFinished { .. } => "GoalFinished",
            Self::MessageAccepted { .. } => "MessageAccepted",
            Self::RunStarted { .. } => "RunStarted",
            Self::AgentSpawned { .. } => "AgentSpawned",
            Self::ProviderFallbackUsed { .. } => "ProviderFallbackUsed",
            Self::ProviderRetryScheduled { .. } => "ProviderRetryScheduled",
            Self::ProviderCallRecorded { .. } => "ProviderCallRecorded",
            Self::GoalProviderCallReserved { .. } => "GoalProviderCallReserved",
            Self::GoalCompactionCallReserved { .. } => "GoalCompactionCallReserved",
            Self::GoalContextCompacted { .. } => "GoalContextCompacted",
            Self::ToolResultsCleared { .. } => "ToolResultsCleared",
            Self::GoalSteeringNoteRecorded { .. } => "GoalSteeringNoteRecorded",
            Self::AgentSteeringNoteRecorded { .. } => "AgentSteeringNoteRecorded",
            Self::AssistantDisplayRecorded { .. } => "AssistantDisplayRecorded",
            Self::ToolStarted { .. } => "ToolStarted",
            Self::SupervisorVerdictRecorded { .. } => "SupervisorVerdictRecorded",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::ToolPermissionDecided { .. } => "ToolPermissionDecided",
            Self::HookStarted { .. } => "HookStarted",
            Self::HookFinished { .. } => "HookFinished",
            Self::AgentFinished { .. } => "AgentFinished",
            Self::WorkerDataChunk { .. } => "WorkerDataChunk",
            Self::WorkerDataSealed { .. } => "WorkerDataSealed",
            Self::WorkerHandoverRecorded { .. } => "WorkerHandoverRecorded",
            Self::WorkerIntegrationRecorded { .. } => "WorkerIntegrationRecorded",
            Self::WorkerHandoverDiscarded { .. } => "WorkerHandoverDiscarded",
            Self::WorkerLeaseReleased { .. } => "WorkerLeaseReleased",
            Self::WorkerVerificationRecorded { .. } => "WorkerVerificationRecorded",
            Self::WorkerStopObserved { .. } => "WorkerStopObserved",
            Self::MessageCommitted { .. } => "MessageCommitted",
            Self::RunFinished { .. } => "RunFinished",
        }
    }

    pub(crate) fn scope(&self) -> (Option<RunId>, Option<AgentRunId>) {
        match self {
            Self::WorkspaceRewindStarted { .. }
            | Self::WorkspaceRewindFinished { .. }
            | Self::SessionStarted { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::SessionRenamed { .. }
            | Self::ProviderSetupEditStarted { .. }
            | Self::ProviderSetupEditFinished { .. }
            | Self::GoalStarted { .. }
            | Self::GoalResumed { .. }
            | Self::GoalFinished { .. } => (None, None),
            Self::GoalRunLinked { run_id, .. }
            | Self::GoalPlanContractRecorded { run_id, .. }
            | Self::GoalCompactionCallReserved { run_id, .. }
            | Self::GoalContextCompacted { run_id, .. }
            | Self::ToolResultsCleared { run_id, .. }
            | Self::GoalSteeringNoteRecorded { run_id, .. }
            | Self::MessageAccepted { run_id, .. }
            | Self::RunStarted { run_id, .. }
            | Self::MessageCommitted { run_id, .. }
            | Self::RunFinished { run_id, .. } => (Some(*run_id), None),
            Self::AgentSteeringNoteRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerDataChunk {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerDataSealed {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerHandoverRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerIntegrationRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerHandoverDiscarded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerLeaseReleased {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerVerificationRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::WorkerStopObserved {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AgentSpawned {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ProviderFallbackUsed {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ProviderRetryScheduled {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ProviderCallRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::GoalProviderCallReserved {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AssistantDisplayRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolStarted {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolPermissionDecided {
                run_id,
                agent_run_id,
                ..
            }
            | Self::HookStarted {
                run_id,
                agent_run_id,
                ..
            }
            | Self::HookFinished {
                run_id,
                agent_run_id,
                ..
            }
            | Self::SupervisorVerdictRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolFinished {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AgentFinished {
                run_id,
                agent_run_id,
                ..
            } => (Some(*run_id), Some(*agent_run_id)),
        }
    }

    pub fn payload(&self) -> Result<String, ReplayError> {
        self.validate()?;
        match self {
            Self::SupervisorVerdictRecorded { verdict, .. } => serialize(verdict),
            Self::WorkspaceRewindStarted { record } => serialize(record),
            Self::WorkspaceRewindFinished { record } => serialize(record),
            Self::SessionStarted {
                title,
                workspace_identity:
                    WorkspaceIdentity {
                        device: workspace_device,
                        inode: workspace_inode,
                    },
            } => serialize(&StartedPayload {
                title: title.clone(),
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
            }),
            Self::SessionRenamed { title } => serialize(&TitlePayload {
                title: title.clone(),
            }),
            Self::ProviderSetupEditStarted {
                operation_id,
                target,
                expected_digest,
            } => serialize(&ProviderSetupEditStartedPayload {
                operation_id: *operation_id,
                target: target.clone(),
                expected_digest: *expected_digest,
            }),
            Self::ProviderSetupEditFinished {
                operation_id,
                resulting_digest,
                disposition,
            } => serialize(&ProviderSetupEditFinishedPayload {
                operation_id: *operation_id,
                resulting_digest: *resulting_digest,
                disposition: *disposition,
            }),
            Self::SessionDefaultChanged { defaults } => serialize(defaults),
            Self::SessionForked {
                title,
                source_session_id,
                source_run_id,
                source_sequence,
                prefix_digest,
            } => serialize(&ForkPayload {
                title: title.clone(),
                source_session_id: *source_session_id,
                source_run_id: *source_run_id,
                source_sequence: *source_sequence,
                prefix_digest: *prefix_digest,
            }),
            Self::ContextCompacted { record, active_ms } => serialize(&ContextCompactedPayload {
                record: record.clone(),
                active_ms: *active_ms,
            }),
            Self::GoalStarted {
                goal_id,
                objective,
                include_paths,
                workspace_identity:
                    WorkspaceIdentity {
                        device: workspace_device,
                        inode: workspace_inode,
                    },
                budget,
                phase,
                autonomous,
                resource_settings,
                continues,
            } => serialize(&GoalStartedPayload {
                goal_id: *goal_id,
                objective: objective.clone(),
                include_paths: include_paths.clone(),
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
                budget: *budget,
                phase: *phase,
                autonomous: *autonomous,
                resource_settings: *resource_settings,
                continues: *continues,
            }),
            Self::GoalRunLinked { goal_id, phase, .. } => serialize(&GoalRunPayload {
                goal_id: *goal_id,
                phase: *phase,
            }),
            Self::GoalResumed { goal_id } => serialize(&GoalIdPayload { goal_id: *goal_id }),
            Self::GoalPlanContractRecorded {
                goal_id, contract, ..
            } => serialize(&GoalContractPayload {
                goal_id: *goal_id,
                contract: contract.clone(),
            }),
            Self::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
                exhausted,
                budget_refusal,
            } => serialize(&GoalFinishedPayload {
                goal_id: *goal_id,
                status: *status,
                active_ms: *active_ms,
                exhausted: *exhausted,
                report: report.clone(),
                budget_refusal: *budget_refusal,
            }),
            Self::MessageAccepted {
                text,
                images,
                skill,
                ..
            } => serialize(&MessagePayload {
                skill: skill.clone(),
                text: text.clone(),
                images: images.clone(),
            }),
            Self::MessageCommitted { text, .. } => serialize(&TextPayload { text: text.clone() }),
            Self::RunStarted {
                config,
                active_deadline_ms,
                ..
            } => serialize(&RunStartedPayload {
                config: config.clone(),
                active_deadline_ms: *active_deadline_ms,
            }),
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                grant,
                definition,
                profile,
                assignment,
                ..
            } => serialize(&SpawnPayload {
                profile: profile.clone(),
                assignment: assignment.clone(),
                definition: definition.clone(),
                role: *role,
                ordinal: *ordinal,
                objective: objective.clone(),
                grant: *grant,
            }),
            Self::ProviderFallbackUsed { fallback, .. } => serialize(fallback),
            Self::ProviderRetryScheduled { retry, .. } => serialize(retry),
            Self::ProviderCallRecorded { record, .. } => serialize(record),
            Self::GoalProviderCallReserved {
                goal_id,
                active_ceiling_ms,
                ..
            }
            | Self::GoalCompactionCallReserved {
                goal_id,
                active_ceiling_ms,
                ..
            } => serialize(&GoalProviderCallPayload {
                goal_id: *goal_id,
                active_ceiling_ms: *active_ceiling_ms,
            }),
            Self::ToolResultsCleared {
                covered_tool_count,
                cleared_bytes,
                ..
            } => serialize(&ToolResultsClearedPayload {
                covered_tool_count: *covered_tool_count,
                cleared_bytes: *cleared_bytes,
            }),
            Self::GoalContextCompacted {
                goal_id,
                covered_tool_count,
                record,
                ..
            } => serialize(&GoalContextCompactionPayload {
                goal_id: *goal_id,
                covered_tool_count: *covered_tool_count,
                record: record.clone(),
            }),
            Self::GoalSteeringNoteRecorded { goal_id, text, .. }
            | Self::AgentSteeringNoteRecorded { goal_id, text, .. } => {
                serialize(&GoalSteeringNotePayload {
                    goal_id: *goal_id,
                    text: text.clone(),
                })
            }
            Self::AssistantDisplayRecorded { chunk, .. } => serialize(chunk),
            Self::ToolStarted {
                intent,
                active_ceiling_ms,
                ..
            } => serialize(&ToolStartedPayload {
                intent: intent.clone(),
                active_ceiling_ms: *active_ceiling_ms,
            }),
            Self::ToolFinished {
                observation,
                checkpoint,
                ..
            } => serialize(&ToolFinishedPayload {
                observation: observation.clone(),
                checkpoint: checkpoint.clone(),
            }),
            Self::ToolPermissionDecided { verdict, .. } => serialize(verdict),
            Self::HookStarted { hook, .. } => serialize(hook),
            Self::HookFinished { outcome, .. } => serialize(outcome),
            Self::AgentFinished {
                disposition,
                summary,
                result,
                worker,
                ..
            } => serialize(&AgentFinishedPayload {
                worker: worker.clone(),
                disposition: *disposition,
                summary: summary.clone(),
                result: result.clone(),
            }),
            Self::WorkerDataChunk { record, .. } => serialize(record),
            Self::WorkerDataSealed { record, .. } => serialize(record),
            Self::WorkerHandoverRecorded { record, .. } => serialize(record),
            Self::WorkerIntegrationRecorded { record, .. } => serialize(record),
            Self::WorkerHandoverDiscarded { record, .. } => serialize(record),
            Self::WorkerLeaseReleased { record, .. } => serialize(record),
            Self::WorkerVerificationRecorded { record, .. } => serialize(record),
            Self::WorkerStopObserved { record, .. } => serialize(record),
            Self::RunFinished {
                failure,
                disposition,
                active_ms,
                ..
            } => serialize(&RunFinishedPayload {
                disposition: *disposition,
                failure: *failure,
                active_ms: *active_ms,
            }),
        }
    }

    pub(crate) fn decode(
        kind: &str,
        version: i64,
        payload: &str,
        run_id: Option<RunId>,
        agent_run_id: Option<AgentRunId>,
    ) -> Result<Self, ReplayError> {
        if payload_version(kind) != Some(version) {
            return Err(ReplayError::UnknownEvent);
        }
        if kind == "SupervisorVerdictRecorded"
            && payload.len() > crate::supervisor::MAX_VERDICT_BYTES
        {
            return Err(ReplayError::InvalidPayload);
        }
        let event = match kind {
            "WorkspaceRewindStarted"
                if run_id.is_none()
                    && agent_run_id.is_none()
                    && payload.len() <= super::rewind::MAX_REWIND_EVENT_BYTES =>
            {
                Self::WorkspaceRewindStarted {
                    record: parse(payload)?,
                }
            }
            "WorkspaceRewindFinished"
                if run_id.is_none()
                    && agent_run_id.is_none()
                    && payload.len() <= super::rewind::MAX_REWIND_EVENT_BYTES =>
            {
                Self::WorkspaceRewindFinished {
                    record: parse(payload)?,
                }
            }
            "WorkerDataChunk" => Self::WorkerDataChunk {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerDataSealed" => Self::WorkerDataSealed {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerHandoverRecorded" => Self::WorkerHandoverRecorded {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerIntegrationRecorded" => Self::WorkerIntegrationRecorded {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerHandoverDiscarded" => Self::WorkerHandoverDiscarded {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerLeaseReleased" => Self::WorkerLeaseReleased {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerVerificationRecorded" => Self::WorkerVerificationRecorded {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },
            "WorkerStopObserved" => Self::WorkerStopObserved {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record: parse(payload)?,
            },

            "SessionStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: StartedPayload = parse(payload)?;
                Self::SessionStarted {
                    title: data.title,
                    workspace_identity: WorkspaceIdentity {
                        device: data.workspace_device,
                        inode: data.workspace_inode,
                    },
                }
            }
            "SessionDefaultChanged" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionDefaultChanged {
                    defaults: parse(payload)?,
                }
            }
            "SessionForked" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ForkPayload = parse(payload)?;
                Self::SessionForked {
                    title: data.title,
                    source_session_id: data.source_session_id,
                    source_run_id: data.source_run_id,
                    source_sequence: data.source_sequence,
                    prefix_digest: data.prefix_digest,
                }
            }
            "ContextCompacted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ContextCompactedPayload = parse(payload)?;
                Self::ContextCompacted {
                    record: data.record,
                    active_ms: data.active_ms,
                }
            }
            "SessionRenamed" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionRenamed {
                    title: parse::<TitlePayload>(payload)?.title,
                }
            }
            "ProviderSetupEditStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ProviderSetupEditStartedPayload = parse(payload)?;
                Self::ProviderSetupEditStarted {
                    operation_id: data.operation_id,
                    target: data.target,
                    expected_digest: data.expected_digest,
                }
            }
            "ProviderSetupEditFinished" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ProviderSetupEditFinishedPayload = parse(payload)?;
                Self::ProviderSetupEditFinished {
                    operation_id: data.operation_id,
                    resulting_digest: data.resulting_digest,
                    disposition: data.disposition,
                }
            }
            "GoalStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: GoalStartedPayload = parse(payload)?;
                Self::GoalStarted {
                    goal_id: data.goal_id,
                    objective: data.objective,
                    include_paths: data.include_paths,
                    workspace_identity: WorkspaceIdentity {
                        device: data.workspace_device,
                        inode: data.workspace_inode,
                    },
                    budget: data.budget,
                    phase: data.phase,
                    autonomous: data.autonomous,
                    resource_settings: data.resource_settings,
                    continues: data.continues,
                }
            }
            "GoalRunLinked" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalRunPayload = parse(payload)?;
                Self::GoalRunLinked {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    phase: data.phase,
                }
            }
            "GoalResumed" if run_id.is_none() && agent_run_id.is_none() => Self::GoalResumed {
                goal_id: parse::<GoalIdPayload>(payload)?.goal_id,
            },
            "GoalPlanContractRecorded" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalContractPayload = parse(payload)?;
                Self::GoalPlanContractRecorded {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    contract: data.contract,
                }
            }
            "GoalFinished" if run_id.is_none() && agent_run_id.is_none() => {
                let data: GoalFinishedPayload = parse(payload)?;
                Self::GoalFinished {
                    goal_id: data.goal_id,
                    status: data.status,
                    active_ms: data.active_ms,
                    report: data.report,
                    exhausted: data.exhausted,
                    budget_refusal: data.budget_refusal,
                }
            }
            "MessageAccepted" if run_id.is_some() && agent_run_id.is_none() => {
                let data: MessagePayload = parse(payload)?;
                Self::MessageAccepted {
                    skill: data.skill,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: data.text,
                    images: data.images,
                }
            }
            "SupervisorVerdictRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                Self::SupervisorVerdictRecorded {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    verdict: parse(payload)?,
                }
            }
            "RunStarted" if run_id.is_some() && agent_run_id.is_none() => {
                let data: RunStartedPayload = parse(payload)?;
                Self::RunStarted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    config: data.config,
                    active_deadline_ms: data.active_deadline_ms,
                }
            }
            "AgentSpawned" if run_id.is_some() && agent_run_id.is_some() => {
                let data: SpawnPayload = parse(payload)?;
                Self::AgentSpawned {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    role: data.role,
                    ordinal: data.ordinal,
                    objective: data.objective,
                    grant: data.grant,
                    definition: data.definition,
                    profile: data.profile,
                    assignment: data.assignment,
                }
            }
            "AgentFinished" if run_id.is_some() && agent_run_id.is_some() => {
                let data: AgentFinishedPayload = parse(payload)?;
                Self::AgentFinished {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    disposition: data.disposition,
                    summary: data.summary,
                    result: data.result,
                    worker: data.worker,
                }
            }
            "ProviderFallbackUsed" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ProviderFallbackUsed {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    fallback: parse(payload)?,
                }
            }
            "ProviderRetryScheduled" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ProviderRetryScheduled {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    retry: parse(payload)?,
                }
            }
            "ProviderCallRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ProviderCallRecorded {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    record: parse(payload)?,
                }
            }
            "GoalProviderCallReserved" if run_id.is_some() && agent_run_id.is_some() => {
                let data: GoalProviderCallPayload = parse(payload)?;
                Self::GoalProviderCallReserved {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    active_ceiling_ms: data.active_ceiling_ms,
                }
            }
            "GoalCompactionCallReserved" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalProviderCallPayload = parse(payload)?;
                Self::GoalCompactionCallReserved {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    active_ceiling_ms: data.active_ceiling_ms,
                }
            }
            "ToolResultsCleared" if run_id.is_some() && agent_run_id.is_none() => {
                let data: ToolResultsClearedPayload = parse(payload)?;
                Self::ToolResultsCleared {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    covered_tool_count: data.covered_tool_count,
                    cleared_bytes: data.cleared_bytes,
                }
            }
            "GoalContextCompacted" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalContextCompactionPayload = parse(payload)?;
                Self::GoalContextCompacted {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    covered_tool_count: data.covered_tool_count,
                    record: data.record,
                }
            }
            "GoalSteeringNoteRecorded" if run_id.is_some() && agent_run_id.is_none() => {
                let data: GoalSteeringNotePayload = parse(payload)?;
                Self::GoalSteeringNoteRecorded {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: data.text,
                }
            }
            "AgentSteeringNoteRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                let data: GoalSteeringNotePayload = parse(payload)?;
                Self::AgentSteeringNoteRecorded {
                    goal_id: data.goal_id,
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: data.text,
                }
            }
            "ToolStarted" if run_id.is_some() && agent_run_id.is_some() => {
                let data: ToolStartedPayload = parse(payload)?;
                Self::ToolStarted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    intent: data.intent,
                    active_ceiling_ms: data.active_ceiling_ms,
                }
            }
            "AssistantDisplayRecorded" if run_id.is_some() && agent_run_id.is_some() => {
                Self::AssistantDisplayRecorded {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    chunk: parse(payload)?,
                }
            }
            "ToolFinished" if run_id.is_some() && agent_run_id.is_some() => {
                let data: ToolFinishedPayload = parse(payload)?;
                Self::ToolFinished {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    observation: data.observation,
                    checkpoint: data.checkpoint,
                }
            }
            "ToolPermissionDecided" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ToolPermissionDecided {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    verdict: parse(payload)?,
                }
            }
            "HookStarted" if run_id.is_some() && agent_run_id.is_some() => Self::HookStarted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                hook: parse(payload)?,
            },
            "HookFinished" if run_id.is_some() && agent_run_id.is_some() => Self::HookFinished {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                outcome: parse(payload)?,
            },
            "MessageCommitted" if run_id.is_some() && agent_run_id.is_none() => {
                Self::MessageCommitted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: parse::<TextPayload>(payload)?.text,
                }
            }
            "RunFinished" if run_id.is_some() && agent_run_id.is_none() => {
                let data: RunFinishedPayload = parse(payload)?;
                Self::RunFinished {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    disposition: data.disposition,
                    failure: data.failure,
                    active_ms: data.active_ms,
                }
            }
            "SessionStarted"
            | "SessionDefaultChanged"
            | "WorkspaceRewindStarted"
            | "WorkspaceRewindFinished"
            | "SessionForked"
            | "ContextCompacted"
            | "SessionRenamed"
            | "GoalStarted"
            | "GoalRunLinked"
            | "GoalResumed"
            | "GoalPlanContractRecorded"
            | "GoalFinished"
            | "MessageAccepted"
            | "RunStarted"
            | "AgentSpawned"
            | "ProviderCallRecorded"
            | "AgentFinished"
            | "GoalCompactionCallReserved"
            | "GoalContextCompacted"
            | "ToolResultsCleared"
            | "GoalSteeringNoteRecorded"
            | "AgentSteeringNoteRecorded"
            | "AssistantDisplayRecorded"
            | "ToolStarted"
            | "ToolFinished"
            | "ToolPermissionDecided"
            | "HookStarted"
            | "HookFinished"
            | "MessageCommitted"
            | "RunFinished" => {
                return Err(ReplayError::InvalidPayload);
            }
            _ => return Err(ReplayError::UnknownEvent),
        };
        event.validate()?;
        Ok(event)
    }
}

pub(crate) fn intent_without_expiry(mut value: serde_json::Value) -> Option<serde_json::Value> {
    let intent = if value.get("intent").is_some() {
        value.get_mut("intent")?
    } else {
        value.get_mut("observation")?.get_mut("intent")?
    };
    let old = expiring_intent_text(intent)?;
    intent.as_object_mut()?.remove("expires_at_ms")?;
    let current: crate::tools::EffectIntent = serde_json::from_value(intent.clone()).ok()?;
    let old_digest = crate::validate::digest(old.as_bytes());
    let new_digest = current.digest();
    replace_intent_pin(&mut value, &old_digest, &new_digest);
    let observation = if value.get("observation").is_some() {
        value.get_mut("observation")?
    } else {
        &mut value
    };
    if observation.get("disposition").is_some()
        && observation
            .as_object_mut()?
            .insert("approval_cause".into(), serde_json::Value::Null)
            .is_some()
    {
        return None;
    }
    Some(value)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindStartedV1 {
    rewind_id: uuid::Uuid,
    target: super::rewind::CheckpointTarget,
    head_sequence: u64,
    prefix_digest: [u8; 32],
    plan_digest: [u8; 32],
    file_count: u32,
    restored_bytes: u64,
    kept: Vec<String>,
    overwrite: Vec<super::rewind::Overwrite>,
    workspace_device: u64,
    workspace_inode: u64,
    policy_digest: [u8; 32],
    enforcement_digest: [u8; 32],
    expires_at_ms: i64,
}

pub(crate) fn rewind_started_v1_payload(payload: &str) -> Option<String> {
    let record = serde_json::from_str::<RewindStartedV1>(payload).ok()?;
    (record.expires_at_ms > 0)
        .then(|| serde_json::to_string(&record).ok())
        .flatten()
}

pub(crate) fn rewind_started_v1_to_v2(mut value: serde_json::Value) -> Option<serde_json::Value> {
    let record = value.as_object_mut()?;
    let expiry = record.remove("expires_at_ms")?.as_i64()?;
    (expiry > 0).then_some(value)
}

pub(crate) fn replace_intent_pin(value: &mut serde_json::Value, old: &[u8; 32], new: &[u8; 32]) {
    rebind_intent_pins(value, &|digest| (digest == old).then_some(*new));
}

pub(crate) fn rebind_intent_pins(
    value: &mut serde_json::Value,
    lookup: &impl Fn(&[u8; 32]) -> Option<[u8; 32]>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "intent_digest"
                        | "source_intent_digest"
                        | "last_intent_digest"
                        | "subject_digest"
                ) && let Ok(old) = serde_json::from_value::<[u8; 32]>(value.clone())
                    && let Some(new) = lookup(&old)
                {
                    *value = serde_json::json!(new);
                } else {
                    rebind_intent_pins(value, lookup);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rebind_intent_pins(value, lookup);
            }
        }
        _ => {}
    }
}

fn restore_expiry(mut text: String, expiry: u64) -> Option<String> {
    let at = text.find(",\"use_count\":")?;
    text.insert_str(at, &format!(",\"expires_at_ms\":{expiry}"));
    Some(text)
}

pub(crate) fn expiring_intent_text(value: &serde_json::Value) -> Option<String> {
    let mut value = value.clone();
    let expiry = value.as_object_mut()?.remove("expires_at_ms")?.as_u64()?;
    if expiry == 0 {
        return None;
    }
    let intent: crate::tools::EffectIntent = serde_json::from_value(value).ok()?;
    let text = serialize(&intent).ok()?;
    restore_expiry(text, expiry)
}

pub(crate) fn expiring_payload(kind: &str, payload: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let intent = if kind == "HookFinished" {
        value.get_mut("observation")?.get_mut("intent")?
    } else {
        value.get_mut("intent")?
    };
    let expiry = intent.as_object_mut()?.remove("expires_at_ms")?.as_u64()?;
    if expiry == 0 {
        return None;
    }
    if matches!(kind, "ToolFinished" | "HookFinished") {
        let observation = if kind == "HookFinished" {
            value.get_mut("observation")?
        } else {
            &mut value
        };
        if observation
            .as_object_mut()?
            .insert("approval_cause".into(), serde_json::Value::Null)
            .is_some()
        {
            return None;
        }
    }
    let payload = value.to_string();
    let text = match kind {
        "ToolStarted" => serialize(&parse::<ToolStartedPayload>(&payload).ok()?).ok()?,
        "ToolFinished" => serialize(&parse::<ToolFinishedPayload>(&payload).ok()?).ok()?,
        "HookStarted" => {
            serialize(&parse::<crate::tools::permissions::HookInvocation>(&payload).ok()?).ok()?
        }
        "HookFinished" => {
            serialize(&parse::<crate::tools::permissions::HookOutcome>(&payload).ok()?).ok()?
        }
        _ => return None,
    };
    let text = text.replacen(",\"approval_cause\":null", "", 1);
    restore_expiry(text, expiry)
}

// The typed text a payload had before its kind only gained the glob variants, or `None`
// when it names them (they did not exist) or does not parse.
pub(crate) fn pre_glob_payload(kind: &str, payload: &str) -> Option<String> {
    before_operations(kind, payload, &["glob"], Some("glob"))
}

pub(crate) fn pre_host_payload(kind: &str, payload: &str) -> Option<String> {
    use crate::tools::permissions::{HookResult, PermissionSource};
    fn again<T: Serialize + for<'de> Deserialize<'de>>(
        payload: &str,
        allowed: impl FnOnce(&T) -> bool,
    ) -> Option<String> {
        let value = parse::<T>(payload).ok()?;
        allowed(&value).then(|| serialize(&value).ok()).flatten()
    }
    fn expiring_again<T: for<'de> Deserialize<'de>>(
        kind: &str,
        payload: &str,
        allowed: impl FnOnce(&T) -> bool,
    ) -> Option<String> {
        let source = expiring_payload(kind, payload)?;
        let current = intent_without_expiry(serde_json::from_str(payload).ok()?)?;
        let value = serde_json::from_value::<T>(current).ok()?;
        allowed(&value).then_some(source)
    }
    fn intent_allowed(intent: &crate::tools::EffectIntent) -> bool {
        intent.host_command.is_none() && !intent.call.is_unguarded()
    }
    fn observation_allowed(observation: &crate::tools::ToolObservation) -> bool {
        intent_allowed(&observation.intent)
            && !matches!(
                observation.attestation,
                Some(crate::tools::Attestation::Unguarded(_))
            )
    }
    fn criterion_allowed(criterion: &GoalSuccessCriterion) -> bool {
        !matches!(criterion, GoalSuccessCriterion::ToolCallSucceeded { call } if call.is_unguarded())
    }
    match kind {
        "RunStarted" => again::<RunStartedPayload>(payload, |data| {
            data.config.tool_policy.as_ref().is_none_or(|policy| {
                policy.host_epoch.is_none()
                    && policy.child_unguarded == crate::tools::UnguardedGrant::Denied
            })
        }),
        "ToolStarted" => {
            expiring_again::<ToolStartedPayload>(kind, payload, |data| intent_allowed(&data.intent))
        }
        "ToolFinished" => expiring_again::<ToolFinishedPayload>(kind, payload, |data| {
            observation_allowed(&data.observation)
        }),
        "ToolPermissionDecided" => {
            again::<crate::tools::permissions::PermissionVerdict>(payload, |verdict| {
                verdict.host.is_none()
                    && verdict.basis != PermissionSource::SessionApproval
                    && verdict.source != PermissionSource::SessionApproval
            })
        }
        "AgentSpawned" => again::<SpawnPayload>(payload, |data| {
            data.grant
                .is_none_or(|grant| grant.unguarded() == crate::tools::UnguardedGrant::Denied)
                && data
                    .assignment
                    .as_ref()
                    .is_none_or(|assignment| criterion_allowed(&assignment.request.criterion))
        }),
        "HookStarted" => {
            expiring_again::<crate::tools::permissions::HookInvocation>(kind, payload, |hook| {
                intent_allowed(&hook.intent)
            })
        }
        "HookFinished" => {
            expiring_again::<crate::tools::permissions::HookOutcome>(kind, payload, |outcome| {
                observation_allowed(&outcome.observation)
                    && !matches!(&outcome.result, HookResult::Narrowed { call } if call.is_unguarded())
            })
        }
        "ProviderCallRecorded" => again::<ProviderCallRecord>(payload, |record| {
            record
                .refusal
                .as_ref()
                .is_none_or(|refusal| refusal.operation != "arany_command_unguarded")
                && record
                    .delegation
                    .iter()
                    .flatten()
                    .all(|assignment| match assignment {
                        ChildAssignment::Researcher { .. } => true,
                        ChildAssignment::Editor { assignment } => {
                            criterion_allowed(&assignment.criterion)
                        }
                    })
        }),
        _ => None,
    }
}

// The same for the typed Git write variants, which a later payload version added.
pub(crate) fn pre_host_git_payload(kind: &str, payload: &str) -> Option<String> {
    fn has_host(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(fields) => {
                fields.contains_key("host")
                    || fields.get("network").is_some_and(|v| v == "host")
                    || fields.values().any(has_host)
            }
            serde_json::Value::Array(values) => values.iter().any(has_host),
            _ => false,
        }
    }
    if has_host(&serde_json::from_str(payload).ok()?) {
        return None;
    }
    before_operations(kind, payload, &["git_push", "gh_pr_create"], None)
}

pub(crate) fn pre_git_write_payload(kind: &str, payload: &str) -> Option<String> {
    before_operations(
        kind,
        payload,
        &["git_commit", "git_branch", "git_switch"],
        None,
    )
}

pub(crate) fn provider_call_v18_payload(payload: &str) -> Option<String> {
    let mut record: ProviderCallRecord = parse(payload).ok()?;
    let old = record.disposition;
    if let Some(refusal) = &record.refusal {
        if old != ProviderCallDisposition::InvalidResponse
            || refusal.cause == ProposalRefusalCause::InvalidPath
        {
            return None;
        }
        record.disposition = ProviderCallDisposition::Rejected;
    }
    record.validate().ok()?;
    record.disposition = old;
    serialize(&record).ok()
}

pub(crate) fn provider_call_v18_to_v19(value: serde_json::Value) -> Option<serde_json::Value> {
    let canonical = provider_call_v18_payload(&value.to_string())?;
    let mut record: ProviderCallRecord = parse(&canonical).ok()?;
    if record.refusal.is_some() {
        record.disposition = ProviderCallDisposition::Rejected;
    }
    serde_json::to_value(record).ok()
}

pub(crate) fn provider_call_v19_payload(payload: &str) -> Option<String> {
    let record: ProviderCallRecord = parse(payload).ok()?;
    (record.failure_reason != Some(crate::provider::ProviderFailureReason::OutcomeDecode)).then(
        || {
            record.validate().ok()?;
            serialize(&record).ok()
        },
    )?
}

pub(crate) fn provider_call_v19_to_v20(value: serde_json::Value) -> Option<serde_json::Value> {
    let canonical = provider_call_v19_payload(&value.to_string())?;
    serde_json::from_str(&canonical).ok()
}

pub(crate) fn pre_multiedit_payload(kind: &str, payload: &str) -> Option<String> {
    let text = if kind == "RunStarted" {
        serialize(&parse::<RunStartedPayload>(payload).ok()?).ok()?
    } else {
        before_operations(kind, payload, &["multiedit"], Some("multiedit"))?
    };
    (!mentions(
        &serde_json::from_str(&text).ok()?,
        &["multiedit"],
        Some("multiedit"),
    ))
    .then_some(text)
}

// The typed text a verdict had before the typed prefix rule source existed, or `None` when it
// names that source (it did not exist) or does not parse.
pub(crate) fn pre_typed_prefix_payload(payload: &str) -> Option<String> {
    let verdict: crate::tools::permissions::PermissionVerdict = parse(payload).ok()?;
    let forged = |source| source == crate::tools::permissions::PermissionSource::TypedPrefixRule;
    (!forged(verdict.basis) && !forged(verdict.source))
        .then(|| serialize(&verdict).ok())
        .flatten()
}

fn before_operations(
    kind: &str,
    payload: &str,
    operations: &[&str],
    worker_label: Option<&str>,
) -> Option<String> {
    fn again<T: Serialize + for<'de> Deserialize<'de>>(payload: &str) -> Option<String> {
        serialize(&parse::<T>(payload).ok()?).ok()
    }
    let text = match kind {
        "ToolStarted" | "ToolFinished" | "HookStarted" | "HookFinished" => {
            expiring_payload(kind, payload)
        }
        "GoalPlanContractRecorded" => again::<GoalContractPayload>(payload),
        "AgentSpawned" => again::<SpawnPayload>(payload),
        "RunStarted" => run_started_v18_payload(payload),
        "ProviderCallRecorded" => again::<ProviderCallRecord>(payload),
        _ => None,
    }?;
    (!mentions(&serde_json::from_str(&text).ok()?, operations, worker_label)).then_some(text)
}

// Checked before the Goal and Message facts that precede RunStarted commit.
pub(crate) fn run_started_fits(config: &RunConfig) -> bool {
    serialize(&RunStartedPayload {
        config: config.clone(),
        active_deadline_ms: Some(u64::MAX),
    })
    .is_ok_and(|payload| payload.len() <= crate::store::MAX_EVENT_BYTES)
}

pub(crate) fn goal_started_v7_to_v8(mut value: serde_json::Value) -> Option<serde_json::Value> {
    let fields = value.as_object_mut()?;
    if fields.contains_key("include_paths") {
        return None;
    }
    fields.insert("include_paths".into(), serde_json::Value::Null);
    Some(value)
}

pub(crate) fn goal_started_v7_payload(payload: &str) -> Option<String> {
    let value = goal_started_v7_to_v8(serde_json::from_str(payload).ok()?)?;
    let text = serialize(&parse::<GoalStartedPayload>(&value.to_string()).ok()?).ok()?;
    let field = r#""include_paths":null,"#;
    (text.matches(field).count() == 1).then(|| text.replacen(field, "", 1))
}

// Version 18 read only the root, so its digest becomes the root source with an unknown name.
pub(crate) fn run_started_v18_to_v19(value: serde_json::Value) -> Option<serde_json::Value> {
    let serde_json::Value::Object(mut payload) = value else {
        return None;
    };
    let serde_json::Value::Object(config) = payload.get_mut("config")? else {
        return None;
    };
    let sources = match config.remove("instruction_digest")? {
        serde_json::Value::Null => serde_json::json!([]),
        digest => serde_json::json!([{"directory": "", "file": null, "digest": digest}]),
    };
    if config.contains_key("instruction_sources") || config.contains_key("launch_directory") {
        return None;
    }
    config.insert("instruction_sources".into(), sources);
    config.insert("launch_directory".into(), serde_json::json!(""));
    Some(serde_json::Value::Object(payload))
}

// Version 19 had no user-instruction field, so no global file was loaded.
pub(crate) fn run_started_v19_to_v20(value: serde_json::Value) -> Option<serde_json::Value> {
    let serde_json::Value::Object(mut payload) = value else {
        return None;
    };
    let serde_json::Value::Object(config) = payload.get_mut("config")? else {
        return None;
    };
    if config.contains_key("user_instruction_digest") {
        return None;
    }
    config.insert("user_instruction_digest".into(), serde_json::Value::Null);
    Some(serde_json::Value::Object(payload))
}

const USER_DIGEST_FIELD: &str = r#""user_instruction_digest":null,"#;

pub(crate) fn goal_finished_v7_to_v8(mut value: serde_json::Value) -> Option<serde_json::Value> {
    let payload = value.as_object_mut()?;
    if payload.contains_key("budget_refusal") {
        return None;
    }
    payload.insert("budget_refusal".into(), serde_json::Value::Null);
    Some(value)
}

pub(crate) fn goal_finished_v7_payload(payload: &str) -> Option<String> {
    let value = goal_finished_v7_to_v8(serde_json::from_str(payload).ok()?)?;
    let text = serialize(&parse::<GoalFinishedPayload>(&value.to_string()).ok()?).ok()?;
    Some(text.replace(r#","budget_refusal":null"#, ""))
}

// Version 19's hashed text: the current text without the null user digest.
pub(crate) fn run_started_v19_payload(payload: &str) -> Option<String> {
    let value = run_started_v19_to_v20(serde_json::from_str(payload).ok()?)?;
    let text = serialize(&parse::<RunStartedPayload>(&value.to_string()).ok()?).ok()?;
    (text.matches(USER_DIGEST_FIELD).count() == 1).then(|| text.replacen(USER_DIGEST_FIELD, "", 1))
}

// Version 18's hashed text: version 19's with the sources written back as one digest.
pub(crate) fn run_started_v18_payload(payload: &str) -> Option<String> {
    let value = run_started_v18_to_v19(serde_json::from_str(payload).ok()?)?;
    let sources: Vec<InstructionSource> =
        serde_json::from_value(value["config"]["instruction_sources"].clone()).ok()?;
    let text = run_started_v19_payload(&value.to_string())?;
    let current = format!(
        r#""instruction_sources":{},"launch_directory":"""#,
        serialize(&sources).ok()?
    );
    let legacy = format!(
        r#""instruction_digest":{}"#,
        serialize(&sources.first().map(|source| source.digest)).ok()?
    );
    (text.matches(&current).count() == 1).then(|| text.replacen(&current, &legacy, 1))
}

fn mentions(value: &serde_json::Value, operations: &[&str], worker_label: Option<&str>) -> bool {
    let named = |field: &serde_json::Value| {
        field.as_str().is_some_and(|name| {
            operations.iter().any(|operation| {
                name == *operation || name.strip_prefix("arany_") == Some(operation)
            })
        })
    };
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, field)| {
            (key == "operation" && named(field))
                || (key == "tools"
                    && worker_label.is_some_and(|label| {
                        field
                            .as_array()
                            .is_some_and(|tools| tools.iter().any(|tool| tool == label))
                    }))
                || mentions(field, operations, worker_label)
        }),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|item| mentions(item, operations, worker_label)),
        _ => false,
    }
}

fn serialize<T: Serialize>(value: &T) -> Result<String, ReplayError> {
    serde_json::to_string(value).map_err(|_| ReplayError::InvalidPayload)
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, ReplayError> {
    serde_json::from_str(value).map_err(|_| ReplayError::InvalidPayload)
}

fn required_skill<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let skill = Option::<String>::deserialize(deserializer)?;
    if skill
        .as_deref()
        .is_some_and(|name| !crate::validate::valid_name(name))
    {
        return Err(serde::de::Error::custom("invalid Skill name"));
    }
    Ok(skill)
}

pub(crate) fn pre_command_selection_payload(kind: &str, payload: &str) -> Option<String> {
    let text = match kind {
        "ToolFinished" => serialize(&parse::<ToolFinishedPayload>(payload).ok()?).ok()?,
        "HookFinished" => {
            serialize(&parse::<crate::tools::permissions::HookOutcome>(payload).ok()?).ok()?
        }
        _ => return None,
    };
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let observation = if kind == "HookFinished" {
        &value["observation"]
    } else {
        &value
    };
    let output = observation["output"].as_str()?;
    if matches!(
        observation["intent"]["call"]["operation"].as_str(),
        Some("command" | "command_unguarded")
    ) && serde_json::from_str::<serde_json::Value>(output)
        .is_ok_and(|value| value.get("selection").is_some())
    {
        return None;
    }
    Some(text)
}

pub(crate) fn add_command_selection(
    kind: &str,
    mut value: serde_json::Value,
) -> Option<serde_json::Value> {
    pre_command_selection_payload(kind, &value.to_string())?;
    let observation = if kind == "HookFinished" {
        value.get_mut("observation")?
    } else {
        &mut value
    };
    if !matches!(
        observation["intent"]["call"]["operation"].as_str(),
        Some("command" | "command_unguarded")
    ) {
        return Some(value);
    }
    let budget = usize::try_from(observation["intent"]["limits"]["result_bytes"].as_u64()?).ok()?;
    let output = observation.get_mut("output")?;
    if output.as_str()?.len() > budget {
        return None;
    }
    let Ok(mut result) = serde_json::from_str::<serde_json::Value>(output.as_str()?) else {
        return Some(value);
    };
    if result.get("changeset").is_some() {
        result
            .as_object_mut()?
            .insert("selection".into(), serde_json::Value::Null);
        let result: crate::tools::types::CommandResult = serde_json::from_value(result).ok()?;
        let mut text = serde_json::to_string(&result).ok()?;
        if text.len() > budget {
            text = result.fit_migrated(budget)?;
        }
        if !serde_json::from_str::<crate::tools::types::CommandResult>(&text)
            .ok()?
            .valid()
        {
            return None;
        }
        *output = text.into();
    }
    Some(value)
}

#[cfg(test)]
mod api_limits_tests;
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
pub(crate) use goal::{
    GoalSuccessState, GoalUsage, STORE_ACTION_OVERHEAD_MS, requested_command_resources,
};
pub use records::{
    AgentDisposition, AgentRole, ChildGrant, CollaborationPolicy, CompactionFailure,
    CompactionRecord, CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory,
    DisplayChunk, EffectTarget, IncludeSource, InstructionFile, InstructionSource,
    MAX_DISPLAY_CHUNK_BYTES, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
    MAX_DISPLAY_TEXT_BYTES, ProposalRefusal, ProposalRefusalCause, ProviderCallDisposition,
    ProviderCallRecord, RecentFile, RunConfig, RunDisposition, RunFailure, SessionDefaults,
    ToolIntentRecord, ToolPreflightRefusal, ToolRefusal, ToolRefusalReason,
};
use validate::bounded;

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::{Uuid, Version};

pub(crate) const MAX_COMPACTION_RECENT_FILES: usize = 8;
pub(crate) const MAX_COMPACTION_RECENT_FILES_BYTES: usize = 4 * 1024;

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

pub(crate) const MAX_CHILD_TOOL_CALLS: usize = 16;
// Every model Tool call takes one inference turn, plus the final report.
pub(crate) const MAX_CHILD_TURNS: u8 = {
    assert!(MAX_CHILD_TOOL_CALLS < u8::MAX as usize);
    MAX_CHILD_TOOL_CALLS as u8 + 1
};
pub(crate) const MAX_STEP_TOOL_CALLS: u8 = 8;
const _: () = assert!(MAX_STEP_TOOL_CALLS as u32 <= u8::MAX as u32);
pub(crate) const MAX_CHILD_RESULT_BYTES: usize = 16 * 1024;
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
    HostEnvironmentCapture {
        record: super::EnvironmentCaptureEvent,
    },
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
    CommandToolProbeStarted {
        probe: Box<crate::tools::CommandToolProbe>,
    },
    CommandToolProbeFinished {
        outcome: Box<crate::tools::permissions::HookOutcome>,
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
    ToolPreflightRefused {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: ToolPreflightRefusal,
    },
    ToolProposed {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: ToolIntentRecord,
        active_ceiling_ms: u64,
    },
    ToolStarted {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: ToolIntentRecord,
        active_ceiling_ms: u64,
        verdict: Option<crate::tools::permissions::PermissionVerdict>,
    },
    ToolActiveTimeReserved {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent_digest: [u8; 32],
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
    WorkerLeaseReleased {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: WorkerLeaseRelease,
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
    RoleRouting,
    Roster,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
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
struct ToolActiveTimePayload {
    intent_digest: [u8; 32],
    active_ceiling_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolStartedPayload {
    intent: ToolIntentRecord,
    active_ceiling_ms: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    verdict: Option<crate::tools::permissions::PermissionVerdict>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolProposedPayload {
    intent: ToolIntentRecord,
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

#[derive(Serialize)]
struct TitlePayloadRef<'a> {
    title: &'a str,
}
#[derive(Serialize)]
struct StartedPayloadRef<'a> {
    title: &'a str,
    workspace_device: u64,
    workspace_inode: u64,
}
#[derive(Serialize)]
struct ForkPayloadRef<'a> {
    title: &'a str,
    source_session_id: SessionId,
    source_run_id: RunId,
    source_sequence: u64,
    prefix_digest: [u8; 32],
}
#[derive(Serialize)]
struct ProviderSetupEditStartedPayloadRef<'a> {
    operation_id: uuid::Uuid,
    target: &'a ProviderSetupTarget,
    expected_digest: Option<[u8; 32]>,
}
#[derive(Serialize)]
struct ContextCompactedPayloadRef<'a> {
    record: &'a CompactionRecord,
    active_ms: Option<u64>,
}
#[derive(Serialize)]
struct GoalStartedPayloadRef<'a> {
    goal_id: GoalId,
    objective: &'a str,
    include_paths: &'a Option<Vec<String>>,
    workspace_device: u64,
    workspace_inode: u64,
    budget: GoalBudget,
    phase: GoalPhase,
    autonomous: bool,
    resource_settings: &'a crate::tools::ToolResourceSettings,
    continues: Option<GoalId>,
}
#[derive(Serialize)]
struct GoalContractPayloadRef<'a> {
    goal_id: GoalId,
    contract: &'a GoalPlanContract,
}
#[derive(Serialize)]
struct GoalFinishedPayloadRef<'a> {
    goal_id: GoalId,
    status: GoalStatus,
    active_ms: u64,
    report: &'a GoalReport,
    exhausted: bool,
    budget_refusal: Option<BudgetRefusal>,
}
#[derive(Serialize)]
struct MessagePayloadRef<'a> {
    skill: &'a Option<String>,
    text: &'a str,
    images: &'a Vec<crate::provider::ImageAttachment>,
}
#[derive(Serialize)]
struct TextPayloadRef<'a> {
    text: &'a str,
}
#[derive(Serialize)]
struct RunStartedPayloadRef<'a> {
    config: &'a RunConfig,
    active_deadline_ms: Option<u64>,
}
#[derive(Serialize)]
struct SpawnPayloadRef<'a> {
    profile: &'a Option<String>,
    assignment: &'a Option<WorkerAssignment>,
    definition: &'a Option<String>,
    role: AgentRole,
    ordinal: u8,
    objective: &'a Option<String>,
    grant: Option<ChildGrant>,
}
#[derive(Serialize)]
struct GoalContextCompactionPayloadRef<'a> {
    goal_id: GoalId,
    covered_tool_count: u32,
    record: &'a CompactionRecord,
}
#[derive(Serialize)]
struct GoalSteeringNotePayloadRef<'a> {
    goal_id: GoalId,
    text: &'a str,
}
#[derive(Serialize)]
struct ToolStartedPayloadRef<'a> {
    intent: &'a ToolIntentRecord,
    active_ceiling_ms: u64,
    verdict: &'a Option<crate::tools::permissions::PermissionVerdict>,
}
#[derive(Serialize)]
struct ToolProposedPayloadRef<'a> {
    intent: &'a ToolIntentRecord,
    active_ceiling_ms: u64,
}
#[derive(Serialize)]
struct ToolFinishedPayloadRef<'a> {
    #[serde(flatten)]
    observation: &'a crate::tools::ToolObservation,
    checkpoint: &'a Option<super::Checkpoint>,
}
#[derive(Serialize)]
struct AgentFinishedPayloadRef<'a> {
    worker: &'a Option<WorkerOutcome>,
    disposition: AgentDisposition,
    summary: Option<&'a str>,
    result: Option<&'a str>,
}

impl Event {
    pub(crate) fn finished_payload_size(summary: &str, result: &str) -> Result<usize, ReplayError> {
        serialize(&AgentFinishedPayloadRef {
            worker: &None,
            disposition: AgentDisposition::Finished,
            summary: Some(summary),
            result: Some(result),
        })
        .map(|payload| payload.len())
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::HostEnvironmentCapture { .. } => "HostEnvironmentCapture",
            Self::WorkspaceRewindStarted { .. } => "WorkspaceRewindStarted",
            Self::WorkspaceRewindFinished { .. } => "WorkspaceRewindFinished",
            Self::SessionStarted { .. } => "SessionStarted",
            Self::SessionDefaultChanged { .. } => "SessionDefaultChanged",
            Self::SessionForked { .. } => "SessionForked",
            Self::ContextCompacted { .. } => "ContextCompacted",
            Self::SessionRenamed { .. } => "SessionRenamed",
            Self::CommandToolProbeStarted { .. } => "CommandToolProbeStarted",
            Self::CommandToolProbeFinished { .. } => "CommandToolProbeFinished",
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
            Self::ToolActiveTimeReserved { .. } => "ToolActiveTimeReserved",
            Self::SupervisorVerdictRecorded { .. } => "SupervisorVerdictRecorded",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::ToolProposed { .. } => "ToolProposed",
            Self::ToolPreflightRefused { .. } => "ToolPreflightRefused",
            Self::HookStarted { .. } => "HookStarted",
            Self::HookFinished { .. } => "HookFinished",
            Self::AgentFinished { .. } => "AgentFinished",
            Self::WorkerLeaseReleased { .. } => "WorkerLeaseReleased",
            Self::WorkerVerificationRecorded { .. } => "WorkerVerificationRecorded",
            Self::WorkerStopObserved { .. } => "WorkerStopObserved",
            Self::MessageCommitted { .. } => "MessageCommitted",
            Self::RunFinished { .. } => "RunFinished",
        }
    }

    pub(crate) fn scope(&self) -> (Option<RunId>, Option<AgentRunId>) {
        match self {
            Self::HostEnvironmentCapture { .. } => (None, None),
            Self::WorkspaceRewindStarted { .. }
            | Self::WorkspaceRewindFinished { .. }
            | Self::SessionStarted { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::SessionRenamed { .. }
            | Self::CommandToolProbeStarted { .. }
            | Self::CommandToolProbeFinished { .. }
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
            | Self::ToolActiveTimeReserved {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolStarted {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolPreflightRefused {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolProposed {
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
            Self::HostEnvironmentCapture { record } => serialize(record),
            Self::SupervisorVerdictRecorded { verdict, .. } => serialize(verdict),
            Self::ToolPreflightRefused { record, .. } => serialize(record),
            Self::WorkspaceRewindStarted { record } => serialize(record),
            Self::WorkspaceRewindFinished { record } => serialize(record),
            Self::SessionStarted {
                title,
                workspace_identity:
                    WorkspaceIdentity {
                        device: workspace_device,
                        inode: workspace_inode,
                    },
            } => serialize(&StartedPayloadRef {
                title,
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
            }),
            Self::SessionRenamed { title } => serialize(&TitlePayloadRef { title }),
            Self::CommandToolProbeStarted { probe } => serialize(probe),
            Self::CommandToolProbeFinished { outcome } => serialize(outcome),
            Self::ProviderSetupEditStarted {
                operation_id,
                target,
                expected_digest,
            } => serialize(&ProviderSetupEditStartedPayloadRef {
                operation_id: *operation_id,
                target,
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
            } => serialize(&ForkPayloadRef {
                title,
                source_session_id: *source_session_id,
                source_run_id: *source_run_id,
                source_sequence: *source_sequence,
                prefix_digest: *prefix_digest,
            }),
            Self::ContextCompacted { record, active_ms } => {
                serialize(&ContextCompactedPayloadRef {
                    record,
                    active_ms: *active_ms,
                })
            }
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
            } => serialize(&GoalStartedPayloadRef {
                goal_id: *goal_id,
                objective,
                include_paths,
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
                budget: *budget,
                phase: *phase,
                autonomous: *autonomous,
                resource_settings,
                continues: *continues,
            }),
            Self::GoalRunLinked { goal_id, phase, .. } => serialize(&GoalRunPayload {
                goal_id: *goal_id,
                phase: *phase,
            }),
            Self::GoalResumed { goal_id } => serialize(&GoalIdPayload { goal_id: *goal_id }),
            Self::GoalPlanContractRecorded {
                goal_id, contract, ..
            } => serialize(&GoalContractPayloadRef {
                goal_id: *goal_id,
                contract,
            }),
            Self::GoalFinished {
                goal_id,
                status,
                active_ms,
                report,
                exhausted,
                budget_refusal,
            } => serialize(&GoalFinishedPayloadRef {
                goal_id: *goal_id,
                status: *status,
                active_ms: *active_ms,
                exhausted: *exhausted,
                report,
                budget_refusal: *budget_refusal,
            }),
            Self::MessageAccepted {
                text,
                images,
                skill,
                ..
            } => serialize(&MessagePayloadRef {
                skill,
                text,
                images,
            }),
            Self::MessageCommitted { text, .. } => serialize(&TextPayloadRef { text }),
            Self::RunStarted {
                config,
                active_deadline_ms,
                ..
            } => serialize(&RunStartedPayloadRef {
                config,
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
            } => serialize(&SpawnPayloadRef {
                profile,
                assignment,
                definition,
                role: *role,
                ordinal: *ordinal,
                objective,
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
            } => serialize(&GoalContextCompactionPayloadRef {
                goal_id: *goal_id,
                covered_tool_count: *covered_tool_count,
                record,
            }),
            Self::GoalSteeringNoteRecorded { goal_id, text, .. }
            | Self::AgentSteeringNoteRecorded { goal_id, text, .. } => {
                serialize(&GoalSteeringNotePayloadRef {
                    goal_id: *goal_id,
                    text,
                })
            }
            Self::AssistantDisplayRecorded { chunk, .. } => serialize(chunk),
            Self::ToolActiveTimeReserved {
                intent_digest,
                active_ceiling_ms,
                ..
            } => serialize(&ToolActiveTimePayload {
                intent_digest: *intent_digest,
                active_ceiling_ms: *active_ceiling_ms,
            }),
            Self::ToolStarted {
                intent,
                active_ceiling_ms,
                verdict,
                ..
            } => serialize(&ToolStartedPayloadRef {
                intent,
                active_ceiling_ms: *active_ceiling_ms,
                verdict,
            }),
            Self::ToolProposed {
                intent,
                active_ceiling_ms,
                ..
            } => serialize(&ToolProposedPayloadRef {
                intent,
                active_ceiling_ms: *active_ceiling_ms,
            }),
            Self::ToolFinished {
                observation,
                checkpoint,
                ..
            } => serialize(&ToolFinishedPayloadRef {
                observation,
                checkpoint,
            }),
            Self::HookStarted { hook, .. } => serialize(hook),
            Self::HookFinished { outcome, .. } => serialize(outcome),
            Self::AgentFinished {
                disposition,
                summary,
                result,
                worker,
                ..
            } => serialize(&AgentFinishedPayloadRef {
                worker,
                disposition: *disposition,
                summary: summary.as_deref(),
                result: result.as_deref(),
            }),
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
        payload: &str,
        run_id: Option<RunId>,
        agent_run_id: Option<AgentRunId>,
    ) -> Result<Self, ReplayError> {
        if kind == "SupervisorVerdictRecorded"
            && payload.len() > crate::supervisor::MAX_VERDICT_BYTES
        {
            return Err(ReplayError::InvalidPayload);
        }
        let event = match kind {
            "HostEnvironmentCapture" if run_id.is_none() && agent_run_id.is_none() => {
                Self::HostEnvironmentCapture {
                    record: parse(payload)?,
                }
            }
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
            "CommandToolProbeStarted" if run_id.is_none() && agent_run_id.is_none() => {
                Self::CommandToolProbeStarted {
                    probe: parse(payload)?,
                }
            }
            "CommandToolProbeFinished" if run_id.is_none() && agent_run_id.is_none() => {
                Self::CommandToolProbeFinished {
                    outcome: parse(payload)?,
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
            "ToolPreflightRefused" if run_id.is_some() && agent_run_id.is_some() => {
                Self::ToolPreflightRefused {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    record: parse(payload)?,
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
            "ToolActiveTimeReserved" if run_id.is_some() && agent_run_id.is_some() => {
                let data: ToolActiveTimePayload = parse(payload)?;
                Self::ToolActiveTimeReserved {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    intent_digest: data.intent_digest,
                    active_ceiling_ms: data.active_ceiling_ms,
                }
            }
            "ToolStarted" if run_id.is_some() && agent_run_id.is_some() => {
                let data: ToolStartedPayload = parse(payload)?;
                Self::ToolStarted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    intent: data.intent,
                    active_ceiling_ms: data.active_ceiling_ms,
                    verdict: data.verdict,
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
            "ToolProposed" if run_id.is_some() && agent_run_id.is_some() => {
                let data: ToolProposedPayload = parse(payload)?;
                Self::ToolProposed {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    intent: data.intent,
                    active_ceiling_ms: data.active_ceiling_ms,
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
            | "ToolActiveTimeReserved"
            | "ToolStarted"
            | "ToolFinished"
            | "ToolProposed"
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

// Checked before the Goal and Message facts that precede RunStarted commit.
pub(crate) fn run_started_fits(config: &RunConfig) -> bool {
    serialize(&RunStartedPayloadRef {
        config,
        active_deadline_ms: Some(u64::MAX),
    })
    .is_ok_and(|payload| payload.len() <= crate::store::MAX_EVENT_BYTES)
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

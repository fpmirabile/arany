mod goal;
mod records;
#[cfg(test)]
mod tests;
mod validate;
use super::worker::*;

use super::{
    MAX_ASSISTANT_MESSAGE_BYTES, MAX_COMPACTION_OUTPUT_TOKENS, MAX_COMPACTION_SUMMARY_BYTES,
    MAX_CONTEXT_CONTENT_BYTES, MAX_HISTORY_RUNS, MAX_STEERING_NOTE_BYTES, MAX_USER_MESSAGE_BYTES,
    compaction,
};

pub use goal::{
    GoalBudget, GoalMcpTool, GoalPhase, GoalPlanContract, GoalReport, GoalSkillBinding, GoalStatus,
    GoalSuccessCriterion, MAX_GOAL_ACTIVE_MS, MAX_GOAL_PLAN_CONTRACT_BYTES,
    MAX_GOAL_PLAN_MCP_TOOLS, MAX_GOAL_PLAN_SKILLS, MAX_GOAL_PROVIDER_CALLS, MAX_GOAL_REPORT_BYTES,
    MAX_GOAL_RUNS,
};
pub(crate) use goal::{GoalSuccessState, GoalUsage, STORE_ACTION_OVERHEAD_MS};
pub use records::{
    AgentDisposition, AgentRole, ChildGrant, CollaborationPolicy, CompactionFailure,
    CompactionRecord, CompactionStatus, ContextUsage, DISPLAY_LIMIT_TEXT, DisplayCategory,
    DisplayChunk, MAX_DISPLAY_CHUNK_BYTES, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
    MAX_DISPLAY_TEXT_BYTES, ProposalRefusal, ProposalRefusalCause, ProviderCallDisposition,
    ProviderCallRecord, RunConfig, RunDisposition, RunFailure, SessionDefaults,
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

pub(crate) fn payload_version(kind: &str) -> Option<i64> {
    match kind {
        "SessionStarted"
        | "SessionDefaultChanged"
        | "SessionForked"
        | "SessionRenamed"
        | "GoalRunLinked"
        | "GoalResumed"
        | "GoalPlanContractRecorded"
        | "GoalContextCompacted"
        | "GoalSteeringNoteRecorded"
        | "AssistantDisplayRecorded"
        | "MessageCommitted" => Some(6),
        "MessageAccepted" => Some(7),
        "AgentSpawned" => Some(11),
        "AgentFinished" => Some(10),
        "ProviderCallRecorded" => Some(12),
        "WorkerDataChunk"
        | "WorkerDataSealed"
        | "WorkerStopObserved"
        | "WorkerIntegrationRecorded"
        | "WorkerHandoverDiscarded"
        | "WorkerLeaseReleased"
        | "WorkerVerificationRecorded" => Some(1),
        "WorkerHandoverRecorded" => Some(2),
        "GoalStarted" | "GoalFinished" => Some(7),
        "SupervisorVerdictRecorded" => Some(1),
        "WorkspaceRewindStarted" | "WorkspaceRewindFinished" | "ProviderFallbackUsed" => Some(1),
        "ToolPermissionDecided" => Some(4),
        "HookStarted" => Some(3),
        "HookFinished" => Some(6),
        "ToolFinished" => Some(16),
        "RunStarted" => Some(16),
        "RunFinished" => Some(9),
        "GoalProviderCallReserved" => Some(7),
        "GoalCompactionCallReserved" => Some(7),
        "ToolStarted" => Some(12),
        "ContextCompacted" => Some(7),
        _ => None,
    }
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
    GoalStarted {
        goal_id: GoalId,
        objective: String,
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
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalProviderCallPayload {
    goal_id: GoalId,
    active_ceiling_ms: u64,
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
            Self::GoalStarted { .. } => "GoalStarted",
            Self::GoalRunLinked { .. } => "GoalRunLinked",
            Self::GoalResumed { .. } => "GoalResumed",
            Self::GoalPlanContractRecorded { .. } => "GoalPlanContractRecorded",
            Self::GoalFinished { .. } => "GoalFinished",
            Self::MessageAccepted { .. } => "MessageAccepted",
            Self::RunStarted { .. } => "RunStarted",
            Self::AgentSpawned { .. } => "AgentSpawned",
            Self::ProviderFallbackUsed { .. } => "ProviderFallbackUsed",
            Self::ProviderCallRecorded { .. } => "ProviderCallRecorded",
            Self::GoalProviderCallReserved { .. } => "GoalProviderCallReserved",
            Self::GoalCompactionCallReserved { .. } => "GoalCompactionCallReserved",
            Self::GoalContextCompacted { .. } => "GoalContextCompacted",
            Self::GoalSteeringNoteRecorded { .. } => "GoalSteeringNoteRecorded",
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
            | Self::GoalStarted { .. }
            | Self::GoalResumed { .. }
            | Self::GoalFinished { .. } => (None, None),
            Self::GoalRunLinked { run_id, .. }
            | Self::GoalPlanContractRecorded { run_id, .. }
            | Self::GoalCompactionCallReserved { run_id, .. }
            | Self::GoalContextCompacted { run_id, .. }
            | Self::GoalSteeringNoteRecorded { run_id, .. }
            | Self::MessageAccepted { run_id, .. }
            | Self::RunStarted { run_id, .. }
            | Self::MessageCommitted { run_id, .. }
            | Self::RunFinished { run_id, .. } => (Some(*run_id), None),
            Self::WorkerDataChunk {
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
            } => serialize(&GoalFinishedPayload {
                goal_id: *goal_id,
                status: *status,
                active_ms: *active_ms,
                exhausted: *exhausted,
                report: report.clone(),
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
            Self::GoalSteeringNoteRecorded { goal_id, text, .. } => {
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
            "GoalStarted" if run_id.is_none() && agent_run_id.is_none() => {
                let data: GoalStartedPayload = parse(payload)?;
                Self::GoalStarted {
                    goal_id: data.goal_id,
                    objective: data.objective,
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
            | "GoalSteeringNoteRecorded"
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
        .is_some_and(|name| !crate::tools::types::valid_name(name))
    {
        return Err(serde::de::Error::custom("invalid Skill name"));
    }
    Ok(skill)
}

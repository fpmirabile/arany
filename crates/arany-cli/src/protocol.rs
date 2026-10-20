pub mod methods;
pub mod rpc;
mod schema;

pub use schema::protocol_schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventLine {
    pub sequence: u64,
    pub session_id: String,
    pub run_id: Option<String>,
    pub agent_run_id: Option<String>,
    #[schemars(schema_with = "event_kind_schema")]
    pub kind: String,
    pub event_version: u32,
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub payload: serde_json::Value,
    pub created_at_ms: i64,
}

impl From<&arany::EventEnvelope> for EventLine {
    fn from(envelope: &arany::EventEnvelope) -> Self {
        Self {
            sequence: envelope.sequence,
            session_id: envelope.session_id.to_string(),
            run_id: envelope.run_id.map(|id| id.to_string()),
            agent_run_id: envelope.agent_run_id.map(|id| id.to_string()),
            kind: envelope.event.kind().into(),
            event_version: u32::try_from(envelope.event.version()).expect("compiled Event version"),
            payload: serde_json::from_str(&envelope.event.payload().expect("validated Event"))
                .expect("valid Event JSON"),
            created_at_ms: envelope.created_at_ms,
        }
    }
}

macro_rules! event_kinds {
    ($($kind:ident),+ $(,)?) => {
        const EVENT_KINDS: &[&str] = &[$(stringify!($kind)),+];

        const _: fn(&arany::Event) = |event| match event {
            $(arany::Event::$kind { .. } => ()),+
        };
    };
}

// The exhaustive match keeps this binary-only catalog in step with Event variants.
event_kinds!(
    SessionStarted,
    SessionDefaultChanged,
    SessionForked,
    ContextCompacted,
    SessionRenamed,
    GoalStarted,
    GoalRunLinked,
    GoalResumed,
    GoalPlanContractRecorded,
    GoalFinished,
    MessageAccepted,
    RunStarted,
    AgentSpawned,
    ProviderFallbackUsed,
    ProviderRetryScheduled,
    ProviderCallRecorded,
    GoalProviderCallReserved,
    GoalCompactionCallReserved,
    GoalContextCompacted,
    ToolResultsCleared,
    GoalSteeringNoteRecorded,
    AssistantDisplayRecorded,
    ToolStarted,
    ToolFinished,
    SupervisorVerdictRecorded,
    ToolPermissionDecided,
    HookStarted,
    HookFinished,
    AgentFinished,
    MessageCommitted,
    RunFinished,
    WorkerDataChunk,
    WorkerDataSealed,
    WorkerStopObserved,
    WorkerHandoverRecorded,
    WorkerIntegrationRecorded,
    WorkerHandoverDiscarded,
    WorkerLeaseReleased,
    WorkerVerificationRecorded,
    WorkspaceRewindStarted,
    WorkspaceRewindFinished,
);

fn event_kind_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "string", "enum": EVENT_KINDS})
}

#[cfg(test)]
mod tests;

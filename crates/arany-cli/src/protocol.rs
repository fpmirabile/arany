pub mod methods;
pub mod rpc;
mod schema;

pub use schema::protocol_schema;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Serialize, JsonSchema, Debug, Clone, PartialEq)]
pub(crate) struct EventLine {
    pub sequence: u64,
    pub session_id: String,
    pub run_id: Option<String>,
    pub agent_run_id: Option<String>,
    #[schemars(schema_with = "event_kind_schema")]
    pub kind: &'static str,
    pub event_version: u32,
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub payload: serde_json::Value,
    pub created_at_ms: i64,
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
    ProviderCallRecorded,
    GoalProviderCallReserved,
    GoalCompactionCallReserved,
    GoalContextCompacted,
    GoalSteeringNoteRecorded,
    AssistantDisplayRecorded,
    ToolStarted,
    ToolFinished,
    ToolPermissionDecided,
    HookStarted,
    HookFinished,
    AgentFinished,
    MessageCommitted,
    RunFinished,
);

fn event_kind_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "string", "enum": EVENT_KINDS})
}

#[cfg(test)]
mod tests;

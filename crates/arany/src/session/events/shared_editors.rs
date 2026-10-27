use super::*;

pub(crate) fn pre_shared_editor_payload(kind: &str, payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let field = match kind {
        "AgentSpawned" => "assignment",
        "AgentFinished" => "worker",
        _ => return None,
    };
    if !value.get(field)?.is_null() {
        return None;
    }
    live_editor_payload(kind, payload)
}

pub(crate) fn live_editor_payload(kind: &str, payload: &str) -> Option<String> {
    if !matches!(kind, "AgentSpawned" | "AgentFinished") {
        return None;
    }
    let event = Event::decode(
        kind,
        payload_version(kind)?,
        payload,
        Some(RunId::new()),
        Some(AgentRunId::new()),
    )
    .ok()?;
    event.payload().ok()
}

pub(crate) fn retired_worker_payload(kind: &str, payload: &str) -> Option<String> {
    fn again<T: Serialize + for<'de> Deserialize<'de>>(payload: &str) -> Option<String> {
        serialize(&parse::<T>(payload).ok()?).ok()
    }
    match kind {
        "ToolProposed" => again::<ToolProposedPayload>(payload),
        "ToolStarted" => again::<ToolStartedPayload>(payload),
        "ToolFinished" => again::<ToolFinishedPayload>(payload),
        "HookStarted" => again::<crate::tools::permissions::HookInvocation>(payload),
        "HookFinished" => again::<crate::tools::permissions::HookOutcome>(payload),
        "GoalPlanContractRecorded" => again::<GoalContractPayload>(payload),
        "ProviderCallRecorded" => again::<ProviderCallRecord>(payload),
        "AgentSpawned" => again::<SpawnPayload>(payload),
        "SupervisorVerdictRecorded" => again::<crate::supervisor::SupervisorVerdict>(payload),
        _ => None,
    }
}

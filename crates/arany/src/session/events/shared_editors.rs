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

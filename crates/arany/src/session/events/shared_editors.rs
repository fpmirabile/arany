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

// Version 24 pinned a Worker data allowance that no Run uses once editors edit the Workspace.
pub(crate) fn run_started_v24_to_v25(mut value: serde_json::Value) -> Option<serde_json::Value> {
    take_worker_allowance(&mut value)?;
    Some(value)
}

fn take_worker_allowance(value: &mut serde_json::Value) -> Option<Option<u64>> {
    match value.get_mut("config")?.get_mut("worker_policy")? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Object(policy) => {
            let bytes = policy.remove("data_bytes")?.as_u64()?;
            (1..=crate::session::worker::WORKER_DATA_BYTES as u64)
                .contains(&bytes)
                .then_some(Some(bytes))
        }
        _ => None,
    }
}

fn with_worker_allowance(
    text: String,
    data: &RunStartedPayload,
    allowance: Option<u64>,
) -> Option<String> {
    let Some(bytes) = allowance else {
        return Some(text);
    };
    let tools = data.config.worker_policy.as_ref()?.max_tools;
    let last = format!("\"max_tools\":{tools}}}");
    (text.matches(&last).count() == 1).then(|| {
        text.replacen(
            &last,
            &format!("\"max_tools\":{tools},\"data_bytes\":{bytes}}}"),
            1,
        )
    })
}

pub(super) fn parse_run_started(
    payload: &str,
    upgrade: fn(serde_json::Value) -> Option<serde_json::Value>,
) -> Option<(RunStartedPayload, Option<u64>)> {
    let limit = crate::store::MAX_EVENT_BYTES;
    let mut value =
        crate::validate::parse_json_bounded(payload.as_bytes(), limit, limit, limit).ok()?;
    let allowance = take_worker_allowance(&mut value)?;
    let value = upgrade(value)?;
    Some((parse(&value.to_string()).ok()?, allowance))
}

pub(super) fn string_run_provider_text(
    data: &RunStartedPayload,
    allowance: Option<u64>,
) -> Option<String> {
    let text = serialize(data).ok()?;
    let tagged = format!("\"provider\":{}", serialize(&data.config.provider).ok()?);
    let original = format!(
        "\"provider\":{}",
        serialize(&data.config.provider.profile()).ok()?
    );
    let text =
        (text.matches(&tagged).count() == 1).then(|| text.replacen(&tagged, &original, 1))?;
    with_worker_allowance(text, data, allowance)
}

pub(crate) fn run_started_v23_payload(payload: &str) -> Option<String> {
    let (data, allowance) = parse_run_started(payload, run_started_v23_to_v24)?;
    string_run_provider_text(&data, allowance)
}

pub(crate) fn run_started_v24_payload(payload: &str) -> Option<String> {
    let (data, allowance) = parse_run_started(payload, Some)?;
    with_worker_allowance(serialize(&data).ok()?, &data, allowance)
}

use super::*;
use serde_json::Value;

fn observation<'a>(kind: &str, value: &'a mut Value) -> Option<&'a mut Value> {
    match kind {
        "ToolFinished" => Some(value),
        "HookFinished" => value.get_mut("observation"),
        _ => None,
    }
}

pub(crate) fn pre_selection_timing_payload(kind: &str, payload: &str) -> Option<String> {
    let text = match kind {
        "ToolFinished" => serialize(&parse::<ToolFinishedPayload>(payload).ok()?).ok()?,
        "HookFinished" => {
            serialize(&parse::<crate::tools::permissions::HookOutcome>(payload).ok()?).ok()?
        }
        _ => return None,
    };
    let mut value: Value = serde_json::from_str(&text).ok()?;
    let observation = observation(kind, &mut value)?;
    if matches!(
        observation["intent"]["call"]["operation"].as_str(),
        Some("command" | "command_unguarded")
    ) && let Ok(result) = serde_json::from_str::<Value>(observation["output"].as_str()?)
        && result["selection"].get("elapsed_ms").is_some()
    {
        return None;
    }
    Some(text)
}

pub(crate) fn add_selection_timing(kind: &str, mut value: Value) -> Option<Value> {
    pre_selection_timing_payload(kind, &value.to_string())?;
    let observation = observation(kind, &mut value)?;
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
    let Ok(mut result) = serde_json::from_str::<Value>(output.as_str()?) else {
        return Some(value);
    };
    if let Some(selection) = result.get_mut("selection").and_then(Value::as_object_mut) {
        selection.insert("elapsed_ms".into(), Value::Null);
        let result: crate::tools::types::CommandResult = serde_json::from_value(result).ok()?;
        let mut text = serialize(&result).ok()?;
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

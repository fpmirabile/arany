use super::*;

pub(crate) fn pre_command_tools_payload(kind: &str, payload: &str) -> Option<String> {
    fn added(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(fields) => {
                fields.contains_key("rewrite")
                    || fields.contains_key("command_tool")
                    || fields.contains_key("command_subject")
                    || fields.contains_key("command_tools_digest")
                    || fields
                        .get("event")
                        .is_some_and(|event| event == "CommandRewrite")
                    || fields.values().any(added)
            }
            serde_json::Value::Array(values) => values.iter().any(added),
            _ => false,
        }
    }
    if added(&serde_json::from_str(payload).ok()?) {
        return None;
    }
    match kind {
        "RunStarted" => serialize(&parse::<RunStartedPayload>(payload).ok()?).ok(),
        "ToolProposed" => serialize(&parse::<ToolProposedPayload>(payload).ok()?).ok(),
        "ToolStarted" => serialize(&parse::<ToolStartedPayload>(payload).ok()?).ok(),
        "ToolFinished" => serialize(&parse::<ToolFinishedPayload>(payload).ok()?).ok(),
        "HookStarted" => {
            serialize(&parse::<crate::tools::permissions::HookInvocation>(payload).ok()?).ok()
        }
        "HookFinished" => {
            serialize(&parse::<crate::tools::permissions::HookOutcome>(payload).ok()?).ok()
        }
        _ => None,
    }
}

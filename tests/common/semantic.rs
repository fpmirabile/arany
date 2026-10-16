//! Rebuilds the semantic request fields from a rendered Responses request, so loopback servers can
//! assert on what the model was shown without depending on the message layout.

use serde_json::{Value, json};

fn parts(wire: &Value) -> Vec<String> {
    if let Some(messages) = wire["messages"].as_array() {
        return messages
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter_map(|part| part["text"].as_str().map(str::to_owned))
            .collect();
    }
    wire["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["role"] == "user")
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part["text"].as_str().map(str::to_owned))
        .collect()
}

fn scoped(part: &str) -> Option<(String, Value)> {
    let value: Value = serde_json::from_str(part).ok()?;
    Some((value["scope"].as_str()?.to_owned(), value["data"].clone()))
}

pub fn input(wire: &Value) -> Value {
    if let Some(text) = wire["input"].as_str() {
        return serde_json::from_str(text).expect("compaction input");
    }
    let system: String = wire["system"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let instructions = wire["instructions"].as_str().unwrap_or(&system);
    let mut semantic = json!({
        "objective": null, "history": [], "includes": [], "workspace_guidance": null,
        "derived_context_summary": null, "child_results": [],
        "goal_budget": null, "goal_plan_contract": null,
    });
    let mut notes = Vec::new();
    for part in parts(wire) {
        if let Some((scope, data)) = scoped(&part) {
            match scope.split(" (").next().unwrap_or_default() {
                "previous_run" => semantic["history"].as_array_mut().unwrap().push(data),
                "includes" => semantic["includes"] = data,
                "workspace_guidance" => semantic["workspace_guidance"] = data,
                "session_summary" => semantic["derived_context_summary"] = data,
                "child_results" => semantic["child_results"] = data,
                "tool_review_request" => semantic["objective"] = data,
                "goal" => {
                    semantic["goal_budget"] = data["budget"].clone();
                    semantic["goal_plan_contract"] = data["plan_contract"].clone();
                }
                "tool_catalog" => semantic["tools"] = json!({"catalog": data}),
                _ => {}
            }
        } else if part.starts_with("Steering notes the user sent") {
            notes.extend(part.lines().skip(1).map(|line| {
                line.split_once("] ")
                    .map_or(line, |(_, note)| note)
                    .to_owned()
            }));
        } else if !part.starts_with("Previous Run (") && !part.starts_with("Image ") {
            semantic["objective"] = json!(part);
        }
    }
    if !notes.is_empty() {
        semantic["steering_notes"] = json!(notes);
    }
    semantic["phase"] = json!(
        if instructions.starts_with("You are a read-only researcher")
            || !instructions.contains("Pinned collaboration policy: ")
                && !instructions.starts_with("You evaluate")
        {
            "child_work"
        } else if instructions.starts_with("You evaluate a proposed coding action") {
            "tool_review"
        } else if semantic["child_results"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        {
            "root_synthesis"
        } else {
            "root_plan"
        }
    );
    if let Some(policy) = instructions
        .split("Pinned collaboration policy: ")
        .nth(1)
        .and_then(|rest| rest.strip_suffix('.'))
    {
        semantic["collaboration"] = serde_json::from_str(policy).unwrap_or(Value::Null);
    }
    semantic
}

#[allow(dead_code)]
/// The model-facing projection of every Tool result in the request, in order, for either route.
pub fn tool_results(wire: &Value) -> Vec<Value> {
    if let Some(messages) = wire["messages"].as_array() {
        return messages
            .iter()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "tool_result")
            .map(|block| serde_json::from_str(block["content"].as_str().unwrap()).unwrap())
            .collect();
    }
    wire["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| serde_json::from_str(item["output"].as_str().unwrap()).unwrap())
        .collect()
}

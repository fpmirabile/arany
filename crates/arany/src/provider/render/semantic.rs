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
    if let Some(text) = wire["input"]
        .as_str()
        .or_else(|| wire["messages"][0]["content"].as_str())
    {
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
                    for key in ["budget", "plan_contract"] {
                        if !data[key].is_null() {
                            semantic[format!("goal_{key}")] = data[key].clone();
                        }
                    }
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

pub fn responses_output(value: &Value) -> Value {
    if value["outcome"]["type"] == "delegate" {
        let mut arguments = value["outcome"].clone();
        arguments
            .as_object_mut()
            .expect("delegation")
            .remove("type");
        json!([{"type":"function_call", "id":"fc_delegate", "call_id":"call_delegate", "status":"completed", "name":"arany_delegate", "arguments":arguments.to_string()}])
    } else {
        json!([{"type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":value.as_str().map_or_else(|| value.to_string(), str::to_owned)}]}])
    }
}

pub fn messages_content(value: &Value) -> Value {
    if value["outcome"]["type"] == "delegate" {
        let mut input = value["outcome"].clone();
        input.as_object_mut().expect("delegation").remove("type");
        json!([{"type":"tool_use", "id":"toolu_delegate", "name":"arany_delegate", "input":input}])
    } else {
        json!([{"type":"text", "text":value.as_str().map_or_else(|| value.to_string(), str::to_owned)}])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Effort, anthropic, openai, render, test_support};

    #[test]
    fn semantic_inverts_render() {
        let request = test_support::read_only_request("gpt-5.4");
        let conversation = render::conversation(&request).unwrap();
        let responses =
            openai::wire::run_body_with_effort(&request, &conversation, Effort::High).unwrap();
        let messages = anthropic::wire::run_body(&request, &conversation, Effort::High).unwrap();
        let input = &request.input;
        let expected = json!({
            "objective": input.objective,
            "history": input
                .history
                .iter()
                .map(|turn| json!({"user": turn.user, "assistant": turn.assistant}))
                .collect::<Vec<_>>(),
            "includes": input.includes,
            "workspace_guidance": input.workspace_guidance,
            "derived_context_summary": input.context_summary,
            "child_results": input
                .child_results
                .iter()
                .map(|item| json!({
                    "objective": item.objective,
                    "summary": item.summary,
                    "result": item.result,
                }))
                .collect::<Vec<_>>(),
            "phase": "root_synthesis",
            "collaboration": request.collaboration,
        });
        for body in [responses, messages] {
            assert_eq!(super::input(&body), expected);
        }
    }
}

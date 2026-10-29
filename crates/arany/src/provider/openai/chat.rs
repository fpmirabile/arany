use crate::provider::conversation::{Block, Conversation, Message, Role, SystemBlock};
use crate::provider::decode::{function_outcome, review_outcome, tool_function};
use crate::provider::tool_specs::{ToolSpec, name_of, outcome_specs};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, ProviderError,
    ProviderOutcome, ProviderRequest, ProviderResponse, ProviderTurn, ProviderWireProvenance,
    ToolUseId, Usage,
};
use serde_json::{Value, json};

const ADAPTER: &str = "openai-chat-completions";

pub(crate) fn run_body(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Option<Effort>,
) -> Result<Value, ProviderError> {
    let mut body = json!({
        "model": request.model,
        "messages": messages(conversation)?,
        "max_completion_tokens": request.max_output_tokens,
        "stream": false,
    });
    let functions: Vec<_> = conversation
        .tools
        .iter()
        .flatten()
        .cloned()
        .chain(outcome_specs(&conversation.outcome))
        .map(function_json)
        .collect();
    let exhausted = request.input.answer_only();
    if request.input.role == crate::provider::PromptRole::ToolReview {
        body["response_format"] = json!({
            "type":"json_schema",
            "json_schema":{
                "name":"arany_outcome",
                "strict":true,
                "schema":conversation.outcome
            }
        });
    } else if !functions.is_empty() {
        body["tools"] = Value::Array(functions);
        body["tool_choice"] = json!(if exhausted { "none" } else { "auto" });
        body["parallel_tool_calls"] = json!(false);
    }
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort.as_str());
    }
    Ok(body)
}

pub(crate) fn compaction_body(request: &CompactionRequest, effort: Option<Effort>) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": [
            {"role":"system", "content": request.prompts.compaction_instructions()},
            {"role":"user", "content": crate::provider::compaction_input(request).to_string()}
        ],
        "max_completion_tokens": request.max_output_tokens,
        "stream": false,
    });
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort.as_str());
    }
    body
}

fn messages(conversation: &Conversation) -> Result<Vec<Value>, ProviderError> {
    let mut output = Vec::new();
    let mut unanswered = Vec::new();
    let instructions = conversation
        .system
        .iter()
        .map(|block| match block {
            SystemBlock::Limits { answer_bytes, .. } => SystemBlock::Limits {
                step_tool_calls: 1,
                answer_bytes: *answer_bytes,
            }
            .text(),
            block => block.text(),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if !instructions.is_empty() {
        output.push(json!({"role":"system", "content":instructions}));
    }
    for message in &conversation.messages {
        match message.role {
            Role::Assistant => {
                let (item, calls) = assistant_item(message)?;
                unanswered.extend(calls);
                output.push(item);
            }
            Role::User => push_user_blocks(&mut output, message, &mut unanswered)?,
        }
    }
    Ok(output)
}

fn assistant_item(message: &Message) -> Result<(Value, Vec<String>), ProviderError> {
    let [Block::AssistantTurn(turn)] = message.blocks.as_slice() else {
        return Err(ProviderError::Rejected);
    };
    let items = turn.items(ADAPTER).ok_or(ProviderError::Rejected)?;
    let [item] = items.as_slice() else {
        return Err(ProviderError::Rejected);
    };
    let delegate = name_of("delegate");
    let mut unanswered = Vec::new();
    if let Some(calls) = item["tool_calls"].as_array() {
        unanswered.extend(calls.iter().filter_map(|call| {
            (call.pointer("/function/name").and_then(Value::as_str) == Some(delegate.as_str()))
                .then(|| call["id"].as_str().map(str::to_owned))
                .flatten()
        }));
    }
    Ok((item.clone(), unanswered))
}

fn push_user_blocks(
    output: &mut Vec<Value>,
    message: &Message,
    unanswered: &mut Vec<String>,
) -> Result<(), ProviderError> {
    for id in std::mem::take(unanswered) {
        if !message.blocks.iter().any(
            |block| matches!(block, Block::ToolResult { id: result, .. } if result.as_str() == id),
        ) {
            output.push(json!({"role":"tool", "tool_call_id":id, "content":"Arany recorded this call; its result follows in a later message."}));
        }
    }
    for block in &message.blocks {
        match block {
            Block::Text(text) => output.push(json!({"role":"user", "content":text})),
            Block::Harness { scope, data } => output.push(json!({
                "role":"user", "content":crate::provider::render::data_text(scope, data)
            })),
            Block::ToolResult {
                id, output: value, ..
            } => output.push(json!({
                "role":"tool", "tool_call_id":id.as_str(), "content":value.to_string()
            })),
            Block::Image { .. } | Block::AssistantTurn(_) => {
                return Err(ProviderError::Rejected);
            }
        }
    }
    Ok(())
}

fn function_json(spec: ToolSpec) -> Value {
    json!({"type":"function","function":{"name":spec.name,"description":spec.description,"parameters":spec.parameters,"strict":true}})
}

pub(crate) fn decode_run(
    bytes: &[u8],
    request: &ProviderRequest,
    secret: &str,
    offered: &[ToolSpec],
    outcome_schema: &Value,
) -> Result<ProviderResponse, ProviderError> {
    let value = crate::provider::parse_response_json(bytes)
        .map_err(|_| ProviderError::InvalidResponseContract)?;
    let response = admit(&value, &request.model, request.max_output_tokens)?;
    let choice = &response.choice;
    let (outcome, turn_message) = decode_outcome(choice, request, offered, outcome_schema)?;
    let turn = ProviderTurn::new(
        ADAPTER,
        &json!([turn_message]).to_string(),
        secret,
        request.max_output_tokens,
    )
    .map_err(|_| ProviderError::InvalidOutcome)?;
    Ok(ProviderResponse {
        outcome,
        turn,
        response_id: Some(response.id),
        usage: response.usage,
        wire_provenance: Some(ProviderWireProvenance::ChatCompletionsStopStorageUnspecified),
    })
}

fn decode_outcome(
    choice: &Value,
    request: &ProviderRequest,
    offered: &[ToolSpec],
    outcome_schema: &Value,
) -> Result<(ProviderOutcome, Value), ProviderError> {
    let message = &choice["message"];
    let calls = tool_calls(message)?;
    match choice["finish_reason"].as_str() {
        Some("stop") if calls.is_none_or(|calls| calls.is_empty()) => {
            decode_answer(message, request)
        }
        Some("tool_calls") if calls.is_some_and(|calls| calls.len() == 1) => {
            if request.input.role == crate::provider::PromptRole::ToolReview {
                return Err(ProviderError::InvalidOutcome);
            }
            let Some(call) = calls.and_then(|calls| calls.first()) else {
                return Err(ProviderError::InvalidOutcome);
            };
            decode_function_call(call, message, offered, outcome_schema)
        }
        _ => Err(ProviderError::InvalidResponseContract),
    }
}

fn tool_calls(message: &Value) -> Result<Option<&[Value]>, ProviderError> {
    match message.get("tool_calls") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(calls)) => Ok(Some(calls)),
        Some(_) => Err(ProviderError::InvalidOutcome),
    }
}

fn decode_answer(
    message: &Value,
    request: &ProviderRequest,
) -> Result<(ProviderOutcome, Value), ProviderError> {
    if message["role"] != "assistant" || !message["refusal"].is_null() {
        return Err(ProviderError::InvalidOutcome);
    }
    let text = message["content"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or(ProviderError::InvalidResponseContract)?;
    let outcome = if request.input.role == crate::provider::PromptRole::ToolReview {
        review_outcome(text)?
    } else {
        ProviderOutcome::Answer(crate::provider::Answer {
            text: text.to_owned(),
        })
    };
    Ok((outcome, json!({"role":"assistant", "content":text})))
}

fn decode_function_call(
    call: &Value,
    message: &Value,
    offered: &[ToolSpec],
    outcome_schema: &Value,
) -> Result<(ProviderOutcome, Value), ProviderError> {
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .ok_or(ProviderError::InvalidResponseContract)?;
    if message["role"] != "assistant"
        || !message["content"].is_null()
        || !message["refusal"].is_null()
        || call["type"] != "function"
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let id = call["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or(ProviderError::InvalidResponseContract)?;
    let arguments = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .ok_or(ProviderError::InvalidResponseContract)?;
    let operation = name
        .strip_prefix("arany_")
        .ok_or(ProviderError::InvalidOutcomeContract)?;
    let fields = crate::tools::parse_json(
        arguments.as_bytes(),
        crate::provider::decode::argument_limit(operation),
    )
    .map_err(|_| ProviderError::InvalidOutcomeContract)?;
    let fields = fields
        .as_object()
        .cloned()
        .ok_or(ProviderError::InvalidOutcomeContract)?;
    let outcome_functions = outcome_specs(outcome_schema);
    if !offered.iter().any(|spec| spec.name == name)
        && !outcome_functions.iter().any(|spec| spec.name == name)
    {
        return Err(ProviderError::InvalidOutcomeContract);
    }
    let outcome = if operation == "delegate" {
        function_outcome(operation, fields)
    } else {
        tool_function(
            ToolUseId::new(id).map_err(|_| ProviderError::InvalidOutcome)?,
            operation,
            Value::Object(fields),
        )
    }
    .map_err(|_| ProviderError::InvalidOutcomeContract)?;
    Ok((
        outcome,
        json!({
            "role":"assistant",
            "content":null,
            "tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]
        }),
    ))
}

pub(crate) fn decode_compaction(
    bytes: &[u8],
    model: &str,
    cap: u32,
) -> Result<CompactionResponse, ProviderError> {
    let response = crate::provider::parse_response_json(bytes)
        .map_err(|_| ProviderError::InvalidResponseContract)?;
    let response = admit(&response, model, cap)?;
    let message = &response.choice["message"];
    if response.choice["finish_reason"] != "stop"
        || message["role"] != "assistant"
        || !message["refusal"].is_null()
        || tool_calls(message)?.is_some_and(|calls| !calls.is_empty())
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let summary = response.choice["message"]["content"]
        .as_str()
        .filter(|text| {
            !text.trim().is_empty() && text.len() <= crate::session::MAX_COMPACTION_SUMMARY_BYTES
        })
        .ok_or(ProviderError::InvalidResponseContract)?
        .to_owned();
    Ok(CompactionResponse {
        summary,
        response_id: Some(response.id),
        usage: response.usage,
        wire_provenance: Some(ProviderWireProvenance::ChatCompletionsStopStorageUnspecified),
    })
}

struct AdmittedResponse {
    id: String,
    usage: Usage,
    choice: Value,
}

fn admit(value: &Value, model: &str, cap: u32) -> Result<AdmittedResponse, ProviderError> {
    let choices = value["choices"]
        .as_array()
        .filter(|choices| choices.len() == 1)
        .ok_or(ProviderError::InvalidResponseContract)?;
    let usage = &value["usage"];
    let input = usage["prompt_tokens"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= MAX_REPORTED_INPUT_TOKENS)
        .ok_or(ProviderError::InvalidResponseContract)?;
    let output = usage["completion_tokens"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= cap)
        .ok_or(ProviderError::InvalidResponseContract)?;
    let total = usage["total_tokens"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(ProviderError::InvalidResponseContract)?;
    let id = value["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or(ProviderError::InvalidResponseContract)?
        .to_owned();
    if value["model"] != model
        || total
            != input
                .checked_add(output)
                .ok_or(ProviderError::InvalidResponseContract)?
    {
        return Err(ProviderError::InvalidResponseContract);
    }
    Ok(AdmittedResponse {
        id,
        usage: Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
        choice: choices[0].clone(),
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests;

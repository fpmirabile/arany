use crate::provider::tool_specs::{ToolSpec, specs};
use crate::provider::{
    COMPACTION_INSTRUCTIONS, compaction_input, outcome_schema, run_input, summary_schema,
};
use crate::provider::{
    CompactionRequest, CompactionResponse, Delegate, Effort, Finish, ProviderError,
    ProviderOutcome, ProviderRequest, ProviderResponse, ProviderWireProvenance,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_REQUEST_BYTES: usize = 512 * 1024;

pub(crate) fn encode_request(request: &Value) -> Result<Vec<u8>, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::Rejected)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ProviderError::Rejected);
    }
    Ok(body)
}

pub(crate) fn decode_run(bytes: &[u8], model: &str) -> Result<ProviderResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    decode_run_response(response, model).map_err(|_| ProviderError::InvalidOutcome)
}

pub(crate) fn decode_streamed_run(
    bytes: &[u8],
    model: &str,
) -> Result<ProviderResponse, ResponseError> {
    decode_run_response(decode_completed_event(bytes)?, model)
}

pub(crate) fn decode_tool_run(
    bytes: &[u8],
    model: &str,
) -> Result<ProviderResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    decode_function_response(response, model).map_err(ResponseError::provider_error)
}

pub(crate) fn decode_streamed_tool_run(
    bytes: &[u8],
    model: &str,
) -> Result<ProviderResponse, ResponseError> {
    decode_function_response(decode_completed_event(bytes)?, model)
}

fn decode_function_response(
    response: WireResponse,
    model: &str,
) -> Result<ProviderResponse, ResponseError> {
    let (response_id, usage, text) = response.completed_function(model)?;
    decode_outcome(response_id, usage, &text)
}

fn decode_completed_event(bytes: &[u8]) -> Result<WireResponse, ResponseError> {
    let event: CompletedEvent =
        serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    if event.kind != "response.completed" {
        return Err(ResponseError::Envelope);
    }
    Ok(event.response)
}

fn decode_run_response(
    response: WireResponse,
    model: &str,
) -> Result<ProviderResponse, ResponseError> {
    let (response_id, usage, text) = response.completed_text(model)?;
    decode_outcome(response_id, usage, &text)
}

fn decode_outcome(
    response_id: String,
    usage: Option<WireUsage>,
    text: &str,
) -> Result<ProviderResponse, ResponseError> {
    let envelope: OutcomeEnvelope =
        serde_json::from_str(text).map_err(|_| ResponseError::Outcome)?;
    let outcome = match envelope.outcome {
        WireOutcome::Finish { summary, result } => {
            ProviderOutcome::Finish(Finish { summary, result })
        }
        WireOutcome::Delegate { children } => ProviderOutcome::Delegate(Delegate { children }),
        WireOutcome::Tool {
            call,
            plan_contract,
        } if call.valid() && !matches!(call, crate::tools::ToolCall::ApplyChangeset { .. }) => {
            ProviderOutcome::Tool {
                call,
                plan_contract,
            }
        }
        WireOutcome::Tool { .. } => return Err(ResponseError::Outcome),
    };
    Ok(ProviderResponse {
        outcome,
        response_id: Some(response_id),
        input_tokens: usage.as_ref().map(|value| value.input_tokens),
        output_tokens: usage.map(|value| value.output_tokens),
        wire_provenance: Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested),
    })
}

#[cfg(test)]
pub(crate) fn outcome_failure_stage(
    bytes: &[u8],
    model: &str,
) -> crate::diagnostics::SubscriptionFailureStage {
    outcome_failure_diagnostic(bytes, model).0
}

pub(crate) fn outcome_failure_diagnostic(
    bytes: &[u8],
    model: &str,
) -> (
    crate::diagnostics::SubscriptionFailureStage,
    Option<crate::diagnostics::ToolArgumentShape>,
) {
    use crate::diagnostics::{SubscriptionFailureStage as Stage, ToolArgumentShape};
    let Ok(response) = decode_completed_event(bytes) else {
        return (Stage::OutcomeContract, None);
    };
    if response.output.iter().any(|item| {
        item.kind == "function_call"
            && !matches!(
                item.name.as_deref(),
                Some("arany_finish" | "arany_delegate")
            )
            && item.arguments.as_ref().is_some_and(|arguments| {
                arguments.len() > crate::tools::types::MAX_TOOL_ARGUMENT_BYTES
            })
    }) {
        return (Stage::OutcomeToolArgumentSize, None);
    }
    let decoded = if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        response.completed_function(model)
    } else {
        response.completed_text(model)
    };
    let Ok((_, _, text)) = decoded else {
        return (Stage::OutcomeContract, None);
    };
    match serde_json::from_str::<OutcomeEnvelope>(&text) {
        Err(error) if error.is_syntax() || error.is_eof() => (Stage::OutcomeEncoding, None),
        Err(_) => (Stage::OutcomeFields, None),
        Ok(OutcomeEnvelope {
            outcome: WireOutcome::Tool { call, .. },
        }) => {
            let stage = match &call {
                crate::tools::ToolCall::Read { limit, .. } if !(1..=4096).contains(limit) => {
                    Stage::OutcomeReadBounds
                }
                _ if !call.valid() => tool_failure_stage(&call),
                _ => Stage::OutcomeContract,
            };
            (stage, Some(ToolArgumentShape::from_call(&call)))
        }
        Ok(_) => (Stage::OutcomeContract, None),
    }
}

fn tool_failure_stage(
    call: &crate::tools::ToolCall,
) -> crate::diagnostics::SubscriptionFailureStage {
    use crate::diagnostics::SubscriptionFailureStage as Stage;
    use crate::tools::{
        ToolCall,
        types::{MAX_TOOL_ARGUMENT_BYTES, valid_digest, valid_name, valid_relative},
    };
    if serde_json::to_vec(call).is_ok_and(|bytes| bytes.len() > MAX_TOOL_ARGUMENT_BYTES) {
        return Stage::OutcomeToolArgumentSize;
    }
    let path = match call {
        ToolCall::List { path } | ToolCall::Search { path, .. } => Some((path, true)),
        ToolCall::Read { path, .. }
        | ToolCall::Mkdir { path }
        | ToolCall::Write { path, .. }
        | ToolCall::Edit { path, .. } => Some((path, false)),
        ToolCall::Command { cwd, .. } => Some((cwd, true)),
        _ => None,
    };
    if path.is_some_and(|(path, root)| !valid_relative(path, root)) {
        return Stage::OutcomeToolPath;
    }
    match call {
        ToolCall::Edit {
            expected_digest, ..
        } if !valid_digest(expected_digest) => Stage::OutcomeToolDigest,
        ToolCall::Write {
            expected_digest: Some(digest),
            ..
        } if !valid_digest(digest) => Stage::OutcomeToolDigest,
        ToolCall::Edit { old, .. } if old.is_empty() => Stage::OutcomeToolEmptyEdit,
        ToolCall::Edit { old, new, .. } if old.len() + new.len() > 8 * 1024 => {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Write { content, .. } if content.len() > 8 * 1024 => Stage::OutcomeToolTextSize,
        ToolCall::Search { query, .. } if query.is_empty() || query.len() > 512 => {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Command { program, .. } if !valid_name(program) => Stage::OutcomeToolProgram,
        _ => Stage::OutcomeToolArguments,
    }
}

#[derive(Deserialize, Serialize)]
struct CompletedEvent {
    #[serde(rename = "type")]
    kind: String,
    response: WireResponse,
}

pub(crate) fn decode_compaction(
    bytes: &[u8],
    model: &str,
) -> Result<CompactionResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    decode_compaction_response(response, model).map_err(|_| ProviderError::InvalidOutcome)
}

fn decode_compaction_response(
    response: WireResponse,
    model: &str,
) -> Result<CompactionResponse, ResponseError> {
    let (response_id, usage, text) = response.completed_text(model)?;
    let summary: SummaryEnvelope =
        serde_json::from_str(&text).map_err(|_| ResponseError::Outcome)?;
    Ok(CompactionResponse {
        summary: summary.summary,
        response_id: Some(response_id),
        input_tokens: usage.as_ref().map(|value| value.input_tokens),
        output_tokens: usage.map(|value| value.output_tokens),
        wire_provenance: Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested),
    })
}

pub(crate) fn decode_streamed_compaction(
    bytes: &[u8],
    model: &str,
) -> Result<CompactionResponse, ResponseError> {
    let response = decode_completed_event(bytes)?;
    decode_compaction_response(response, model)
}

pub(crate) fn run_body(request: &ProviderRequest) -> Value {
    run_body_inner(request, None)
}

pub(crate) fn run_body_with_effort(request: &ProviderRequest, effort: Effort) -> Value {
    run_body_inner(request, Some(effort))
}

fn run_body_inner(request: &ProviderRequest, effort: Option<Effort>) -> Value {
    let mut input = run_input(request);
    if request.tools.is_some() {
        input["tools"]
            .as_object_mut()
            .expect("Tool projection")
            .remove("observations");
    }
    let schema = crate::provider::restrict_outcome_schema(outcome_schema(), request);
    let instructions = crate::provider::run_instructions(
        crate::provider::instruction_base(
            request,
            crate::provider::prompts::OPENAI_TOOL_INSTRUCTIONS,
        ),
        request,
    );
    let mut body = response_body(
        &request.model,
        &instructions,
        input,
        request.max_output_tokens,
        effort,
        "arany_outcome",
        schema,
    );
    if !request.images.is_empty() {
        let mut content = Vec::with_capacity(request.images.len() * 2 + 1);
        for (index, value) in request.images.iter().enumerate() {
            content.push(json!({"type": "input_text", "text": value.label(index)}));
            content.push(json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{}", value.image.base64()),
                "detail": "auto",
            }));
        }
        content.push(json!({"type": "input_text", "text": body["input"].take()}));
        body["input"] = json!([{"role": "user", "content": content}]);
    }
    if let Some(context) = &request.tools {
        body.as_object_mut().expect("request object").remove("text");
        body["tools"] = json!(
            specs(request)
                .into_iter()
                .map(function_json)
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = json!("required");
        body["parallel_tool_calls"] = json!(false);
        let mut input = match body["input"].take() {
            Value::Array(items) => items,
            text => vec![json!({"role":"user","content":text})],
        };
        let projection = context.model_input();
        for (index, observation) in projection["observations"]
            .as_array()
            .expect("observation array")
            .iter()
            .enumerate()
        {
            if observation["call"]["operation"] == "apply_changeset" {
                input.push(json!({"role":"user","content":json!({"engine_integration":observation}).to_string()}));
                continue;
            }
            let mut arguments = observation["call"].clone();
            let operation = arguments
                .as_object_mut()
                .expect("typed call")
                .remove("operation")
                .expect("typed operation");
            let call_id = format!("arany_action_{index}");
            input.push(json!({"type":"function_call","call_id":call_id,"name":format!("arany_{}",operation.as_str().expect("operation name")),"arguments":arguments.to_string(),"status":"completed"}));
            input.push(json!({"type":"function_call_output","call_id":call_id,"output":observation.to_string()}));
        }
        body["input"] = json!(input);
    }
    body
}

fn function_json(spec: ToolSpec) -> Value {
    json!({"type":"function","name":spec.name,"description":spec.description,"parameters":spec.parameters,"strict":true})
}

pub(crate) fn compaction_body(request: &CompactionRequest) -> Value {
    compaction_body_inner(request, None)
}

pub(crate) fn compaction_body_with_effort(request: &CompactionRequest, effort: Effort) -> Value {
    compaction_body_inner(request, Some(effort))
}

fn compaction_body_inner(request: &CompactionRequest, effort: Option<Effort>) -> Value {
    let input = compaction_input(request);
    response_body(
        &request.model,
        COMPACTION_INSTRUCTIONS,
        input,
        request.max_output_tokens,
        effort,
        "arany_compaction",
        summary_schema(),
    )
}

fn response_body(
    model: &str,
    instructions: &str,
    input: Value,
    max_output_tokens: u32,
    effort: Option<Effort>,
    schema_name: &str,
    schema: Value,
) -> Value {
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input.to_string(),
        "max_output_tokens": max_output_tokens,
        "store": false,
        "truncation": "disabled",
        "tools": [],
        "tool_choice": "none",
        "text": {"format": {"type": "json_schema", "name": schema_name, "strict": true, "schema": schema}},
    });
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort.as_str()});
    }
    body
}

#[derive(Deserialize, Serialize)]
struct WireResponse {
    id: String,
    status: String,
    model: String,
    output: Vec<WireItem>,
    usage: Option<WireUsage>,
    error: Option<Value>,
    incomplete_details: Option<Value>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct WireItem {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    role: Option<String>,
    status: Option<String>,
    phase: Option<String>,
    content: Option<Vec<WireContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponseError {
    Envelope,
    Model,
    Status,
    Identifier,
    Message,
    Content,
    Phase,
    MissingFinalMessage,
    DuplicateFinalMessage,
    AmbiguousFinalMessage,
    LateCommentary,
    Outcome,
}

impl ResponseError {
    pub(crate) fn provider_error(self) -> ProviderError {
        match self {
            Self::Outcome => ProviderError::InvalidOutcomeContract,
            _ => ProviderError::InvalidResponseContract,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Clone)]
struct WireContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct WireUsage {
    input_tokens: u32,
    output_tokens: u32,
}

impl WireItem {
    pub(super) fn completed_id(&self) -> Option<&str> {
        self.id.as_deref().filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id.bytes().all(|byte| byte.is_ascii_graphic())
                && (!matches!(self.kind.as_str(), "message" | "function_call")
                    || self.status.as_deref() == Some("completed"))
        })
    }
}

pub(super) fn reconcile_streamed_output(
    bytes: &[u8],
    response_id: &str,
    items: Vec<WireItem>,
) -> Result<Vec<u8>, ResponseError> {
    let mut event: CompletedEvent =
        serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    if event.response.id != response_id {
        return Err(ResponseError::Identifier);
    }
    if event.response.output.is_empty() {
        event.response.output = items;
    } else if event.response.output != items {
        return Err(ResponseError::Message);
    }
    serde_json::to_vec(&event).map_err(|_| ResponseError::Envelope)
}

impl WireResponse {
    fn completed_function(
        self,
        model: &str,
    ) -> Result<(String, Option<WireUsage>, String), ResponseError> {
        self.validate_envelope(model)?;
        let mut outcome = None;
        for item in &self.output {
            match item.kind.as_str() {
                "reasoning" => {}
                "function_call"
                    if outcome.is_none() && item.status.as_deref() == Some("completed") =>
                {
                    if item.content.is_some()
                        || item.phase.is_some()
                        || item.role.is_some()
                        || item.completed_id().is_none()
                        || item.call_id.as_deref().is_none_or(|id| {
                            id.is_empty()
                                || id.len() > 128
                                || !id.bytes().all(|byte| byte.is_ascii_graphic())
                        })
                    {
                        return Err(ResponseError::Message);
                    }
                    let operation = item
                        .name
                        .as_deref()
                        .and_then(|name| name.strip_prefix("arany_"))
                        .ok_or(ResponseError::Outcome)?;
                    let arguments = item
                        .arguments
                        .as_deref()
                        .filter(|args| {
                            args.len()
                                <= if matches!(operation, "finish" | "delegate") {
                                    crate::store::MAX_EVENT_BYTES
                                } else {
                                    crate::tools::types::MAX_TOOL_ARGUMENT_BYTES
                                }
                        })
                        .ok_or(ResponseError::Outcome)?
                        .trim();
                    let Value::Object(mut fields) = crate::tools::parse_json(
                        arguments.as_bytes(),
                        crate::store::MAX_EVENT_BYTES,
                    )
                    .map_err(|_| ResponseError::Outcome)?
                    else {
                        return Err(ResponseError::Outcome);
                    };
                    let text = match operation {
                        "finish" | "delegate" => {
                            if fields.insert("type".into(), json!(operation)).is_some() {
                                return Err(ResponseError::Outcome);
                            }
                            json!({"outcome": fields}).to_string()
                        }
                        "list" | "read" | "search" | "mkdir" | "write" | "edit" | "command"
                        | "skill" | "mcp_list" | "mcp_call" | "git_status" | "git_diff"
                        | "git_log" => {
                            let plan_contract = fields.remove("plan_contract");
                            if fields
                                .insert("operation".into(), json!(operation))
                                .is_some()
                            {
                                return Err(ResponseError::Outcome);
                            }
                            json!({"outcome": {"type": "tool", "call": fields, "plan_contract": plan_contract}}).to_string()
                        }
                        _ => return Err(ResponseError::Outcome),
                    };
                    outcome = Some(text);
                }
                _ => return Err(ResponseError::Message),
            }
        }
        Ok((
            self.id,
            self.usage,
            outcome.ok_or(ResponseError::MissingFinalMessage)?,
        ))
    }

    fn validate_envelope(&self, model: &str) -> Result<(), ResponseError> {
        if self.status != "completed" || self.error.is_some() || self.incomplete_details.is_some() {
            return Err(ResponseError::Status);
        }
        if self.model != model {
            return Err(ResponseError::Model);
        }
        if self.id.is_empty()
            || self.id.len() > 128
            || !self.id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ResponseError::Identifier);
        }
        Ok(())
    }
    fn completed_text(
        self,
        model: &str,
    ) -> Result<(String, Option<WireUsage>, String), ResponseError> {
        self.validate_envelope(model)?;
        let mut text = None;
        for item in self.output {
            match item.kind.as_str() {
                "reasoning" => {}
                "message"
                    if item.role.as_deref() == Some("assistant")
                        && item
                            .status
                            .as_deref()
                            .is_none_or(|status| status == "completed") =>
                {
                    let content = item
                        .content
                        .filter(|content| !content.is_empty())
                        .ok_or(ResponseError::Content)?;
                    let mut message = String::new();
                    for part in content {
                        if part.kind != "output_text" {
                            return Err(ResponseError::Content);
                        }
                        message.push_str(&part.text.ok_or(ResponseError::Content)?);
                    }
                    match item.phase.as_deref() {
                        Some("commentary") if text.is_none() => {}
                        None | Some("final_answer") => {
                            if let Some((_, explicit)) = text {
                                return Err(if explicit && item.phase.is_some() {
                                    ResponseError::DuplicateFinalMessage
                                } else {
                                    ResponseError::AmbiguousFinalMessage
                                });
                            }
                            text = Some((message, item.phase.is_some()));
                        }
                        Some("commentary") => return Err(ResponseError::LateCommentary),
                        Some(_) => return Err(ResponseError::Phase),
                    }
                }
                _ => return Err(ResponseError::Message),
            }
        }
        Ok((
            self.id,
            self.usage,
            text.ok_or(ResponseError::MissingFinalMessage)?.0,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeEnvelope {
    outcome: WireOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum WireOutcome {
    Finish {
        summary: String,
        result: String,
    },
    Delegate {
        children: Vec<String>,
    },
    Tool {
        call: crate::tools::ToolCall,
        #[serde(default)]
        plan_contract: Option<Box<crate::session::GoalPlanContract>>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryEnvelope {
    summary: String,
}

pub(super) fn is_structured_text(text: &str) -> bool {
    serde_json::from_str::<OutcomeEnvelope>(text).is_ok()
        || serde_json::from_str::<SummaryEnvelope>(text).is_ok()
}

#[cfg(test)]
mod tests;

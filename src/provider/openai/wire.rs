use crate::provider::conversation::{Block, Conversation, Role};
use crate::provider::decode::{function_outcome, structured_outcome, tool_request, tools_outcome};
use crate::provider::prompts::COMPACTION_INSTRUCTIONS;
use crate::provider::tool_specs::{NAME_PREFIX, ToolSpec, outcome_specs};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, ProviderError, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, ProviderWireProvenance, ToolUseId, Usage,
};
use crate::provider::{compaction_input, summary_schema};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const ADAPTER: &str = "openai";
const MAX_REQUEST_BYTES: usize = 512 * 1024;
const DELEGATED_OUTPUT: &str = "Arany recorded this call; its result follows in a later message.";

pub(crate) fn encode_request(request: &Value) -> Result<Vec<u8>, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::Rejected)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ProviderError::Rejected);
    }
    Ok(body)
}

/// What a response must satisfy before any outcome is read from it.
pub(crate) struct RunScope<'a> {
    pub(crate) model: &'a str,
    pub(crate) output_cap: u32,
    pub(crate) secret: &'a str,
    /// Names of the Tool and outcome functions the request offered; `None` for a text-only request.
    functions: Option<Vec<String>>,
}

impl<'a> RunScope<'a> {
    pub(crate) fn new(
        request: &'a ProviderRequest,
        conversation: &Conversation,
        secret: &'a str,
    ) -> Self {
        Self {
            model: &request.model,
            output_cap: request.max_output_tokens,
            secret,
            functions: conversation.tools.as_ref().map(|tools| {
                tools
                    .iter()
                    .cloned()
                    .chain(outcome_specs(&conversation.outcome))
                    .map(|spec| spec.name)
                    .collect()
            }),
        }
    }
}

pub(crate) fn decode_run(
    bytes: &[u8],
    scope: &RunScope,
) -> Result<ProviderResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    decode(response, &raw["output"], scope).map_err(|error| {
        if scope.functions.is_some() {
            error.provider_error()
        } else {
            ProviderError::InvalidOutcome
        }
    })
}

pub(crate) fn decode_streamed_run(
    bytes: &[u8],
    scope: &RunScope,
) -> Result<ProviderResponse, ResponseError> {
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    decode(
        decode_completed_event(bytes)?,
        &raw["response"]["output"],
        scope,
    )
}

fn decode_completed_event(bytes: &[u8]) -> Result<WireResponse, ResponseError> {
    let event: CompletedEvent =
        serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    if event.kind != "response.completed" {
        return Err(ResponseError::Envelope);
    }
    Ok(event.response)
}

fn decode(
    response: WireResponse,
    output: &Value,
    scope: &RunScope,
) -> Result<ProviderResponse, ResponseError> {
    let (response_id, usage, outcome) = match &scope.functions {
        Some(offered) => {
            let (id, usage, call) = response.completed_function(scope.model)?;
            (id, usage, call.outcome(offered)?)
        }
        None => {
            let (id, usage, text) = response.completed_text(scope.model)?;
            let outcome = structured_outcome(&text).map_err(|_| ResponseError::Outcome)?;
            (id, usage, outcome)
        }
    };
    let turn = ProviderTurn::new(ADAPTER, &output.to_string(), scope.secret, scope.output_cap)
        .map_err(|_| ResponseError::Outcome)?;
    Ok(ProviderResponse {
        outcome,
        turn,
        response_id: Some(response_id),
        usage: provider_usage(usage.as_ref()),
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
                item.name
                    .as_deref()
                    .and_then(|name| name.strip_prefix(NAME_PREFIX)),
                Some("finish" | "delegate")
            )
            && item.arguments.as_ref().is_some_and(|arguments| {
                arguments.len() > crate::tools::types::MAX_TOOL_ARGUMENT_BYTES
            })
    }) {
        return (Stage::OutcomeToolArgumentSize, None);
    }
    if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        let Ok((_, _, call)) = response.completed_function(model) else {
            return (Stage::OutcomeContract, None);
        };
        let Some(operation) = call.name.strip_prefix(NAME_PREFIX) else {
            return (Stage::OutcomeContract, None);
        };
        let Ok(mut fields) = call.fields() else {
            return (Stage::OutcomeContract, None);
        };
        if matches!(operation, "finish" | "delegate") {
            return match function_outcome(operation, fields) {
                Err(_) => (Stage::OutcomeFields, None),
                Ok(_) => (Stage::OutcomeContract, None),
            };
        }
        fields.remove("plan_contract");
        fields.insert("operation".into(), json!(operation));
        return match serde_json::from_value::<crate::tools::ToolCall>(Value::Object(fields)) {
            Err(_) => (Stage::OutcomeFields, None),
            Ok(call) => {
                let stage = match &call {
                    crate::tools::ToolCall::Read { limit, .. } if !(1..=4096).contains(limit) => {
                        Stage::OutcomeReadBounds
                    }
                    _ if !call.valid() => tool_failure_stage(&call),
                    _ => Stage::OutcomeContract,
                };
                (stage, Some(ToolArgumentShape::from_call(&call)))
            }
        };
    }
    let Ok((_, _, text)) = response.completed_text(model) else {
        return (Stage::OutcomeContract, None);
    };
    match serde_json::from_str::<Value>(&text) {
        Err(_) => (Stage::OutcomeEncoding, None),
        Ok(_) if structured_outcome(&text).is_err() => (Stage::OutcomeFields, None),
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

#[derive(Deserialize)]
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
        usage: provider_usage(usage.as_ref()),
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

pub(crate) fn run_body(
    request: &ProviderRequest,
    conversation: &Conversation,
) -> Result<Value, ProviderError> {
    run_body_inner(request, conversation, None)
}

pub(crate) fn run_body_with_effort(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Effort,
) -> Result<Value, ProviderError> {
    run_body_inner(request, conversation, Some(effort))
}

/// Tools lead the request and the outcome functions follow them, because only the outcomes change
/// with the phase; every Tool and previous item is replayed unchanged ahead of the new ones.
fn run_body_inner(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Option<Effort>,
) -> Result<Value, ProviderError> {
    let instructions = conversation
        .system
        .iter()
        .map(|block| block.text())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut body = response_body(
        &request.model,
        &instructions,
        json!(input_items(conversation)?),
        request.max_output_tokens,
        effort,
        "arany_outcome",
        conversation.outcome.clone(),
    );
    if let Some(tools) = &conversation.tools {
        body.as_object_mut().expect("request object").remove("text");
        body["tools"] = tools
            .iter()
            .cloned()
            .chain(outcome_specs(&conversation.outcome))
            .map(function_json)
            .collect();
        body["tool_choice"] = json!("required");
        body["parallel_tool_calls"] = json!(false);
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    Ok(body)
}

fn input_items(conversation: &Conversation) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    let mut unanswered: Vec<String> = Vec::new();
    for message in &conversation.messages {
        match message.role {
            Role::Assistant => {
                let [Block::AssistantTurn(turn)] = message.blocks.as_slice() else {
                    return Err(ProviderError::Rejected);
                };
                let replay = turn.items(ADAPTER).ok_or(ProviderError::Rejected)?;
                unanswered = replay
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .filter_map(|item| item["call_id"].as_str().map(str::to_owned))
                    .collect();
                items.extend(replay);
            }
            Role::User => {
                let answered = |call_id: &str| {
                    message.blocks.iter().any(
                        |block| matches!(block, Block::ToolResult { id, .. } if id.as_str() == call_id),
                    )
                };
                for call_id in std::mem::take(&mut unanswered) {
                    if !answered(&call_id) {
                        items.push(json!({"type": "function_call_output", "call_id": call_id, "output": DELEGATED_OUTPUT}));
                    }
                }
                let mut parts = Vec::new();
                for block in &message.blocks {
                    match block {
                        Block::Text(text) if text.is_empty() => {}
                        Block::Text(text) => {
                            parts.push(json!({"type": "input_text", "text": text}))
                        }
                        Block::Image { label, image } => {
                            parts.push(json!({"type": "input_text", "text": label}));
                            parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:image/png;base64,{}", image.base64()),
                                "detail": "auto",
                            }));
                        }
                        Block::ToolResult { id, output, .. } => {
                            if !parts.is_empty() {
                                items.push(
                                    json!({"role": "user", "content": std::mem::take(&mut parts)}),
                                );
                            }
                            items.push(json!({"type": "function_call_output", "call_id": id.as_str(), "output": output.to_string()}));
                        }
                        Block::AssistantTurn(_) => return Err(ProviderError::Rejected),
                    }
                }
                if !parts.is_empty() {
                    items.push(json!({"role": "user", "content": parts}));
                }
            }
        }
    }
    Ok(items)
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
        json!(input.to_string()),
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
        "input": input,
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

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    status: String,
    model: String,
    output: Vec<WireItem>,
    usage: Option<WireUsage>,
    error: Option<Value>,
    incomplete_details: Option<Value>,
}

#[derive(Clone, Deserialize, PartialEq)]
pub(super) struct WireItem {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    role: Option<String>,
    status: Option<String>,
    phase: Option<String>,
    content: Option<Vec<WireContent>>,
    name: Option<String>,
    arguments: Option<String>,
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

#[derive(Deserialize, PartialEq, Clone)]
struct WireContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<WireInputTokensDetails>,
}

/// `input_tokens` already includes the cached part; cache writes are not reported on this route.
#[derive(Deserialize)]
struct WireInputTokensDetails {
    cached_tokens: u32,
}

fn provider_usage(usage: Option<&WireUsage>) -> Usage {
    Usage {
        input_tokens: usage.map(|value| value.input_tokens),
        output_tokens: usage.map(|value| value.output_tokens),
        cache_read_input_tokens: usage
            .and_then(|value| value.input_tokens_details.as_ref())
            .map(|details| details.cached_tokens),
        cache_creation_input_tokens: None,
    }
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

/// Completed output items of a streamed response are kept raw beside their typed view, so the
/// replayed turn carries everything the Provider sent, including opaque reasoning ciphertext.
pub(super) fn reconcile_streamed_output(
    bytes: &[u8],
    response_id: &str,
    items: Vec<(WireItem, Value)>,
) -> Result<Vec<u8>, ResponseError> {
    let typed: CompletedEvent =
        serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    if typed.response.id != response_id {
        return Err(ResponseError::Identifier);
    }
    let mut event: Value = serde_json::from_slice(bytes).map_err(|_| ResponseError::Envelope)?;
    if typed.response.output.is_empty() {
        event["response"]["output"] = Value::Array(items.into_iter().map(|(_, raw)| raw).collect());
    } else if typed.response.output.len() != items.len()
        || typed
            .response
            .output
            .iter()
            .zip(&items)
            .any(|(completed, (done, _))| completed != done)
    {
        return Err(ResponseError::Message);
    }
    serde_json::to_vec(&event).map_err(|_| ResponseError::Envelope)
}

/// The one completed function call of a Tool-enabled response.
struct FunctionCall {
    name: String,
    arguments: String,
    call_id: String,
}

impl FunctionCall {
    fn operation(&self) -> Result<&str, ResponseError> {
        self.name
            .strip_prefix(NAME_PREFIX)
            .ok_or(ResponseError::Outcome)
    }

    fn fields(&self) -> Result<Map<String, Value>, ResponseError> {
        let limit = if matches!(self.operation()?, "finish" | "delegate") {
            crate::store::MAX_EVENT_BYTES
        } else {
            crate::tools::types::MAX_TOOL_ARGUMENT_BYTES
        };
        let arguments = self.arguments.trim();
        if arguments.len() > limit {
            return Err(ResponseError::Outcome);
        }
        match crate::tools::parse_json(arguments.as_bytes(), crate::store::MAX_EVENT_BYTES) {
            Ok(Value::Object(fields)) => Ok(fields),
            _ => Err(ResponseError::Outcome),
        }
    }

    fn outcome(&self, offered: &[String]) -> Result<ProviderOutcome, ResponseError> {
        let operation = self.operation()?;
        if !offered.contains(&self.name) {
            return Err(ResponseError::Outcome);
        }
        let fields = self.fields()?;
        match operation {
            "finish" | "delegate" => function_outcome(operation, fields),
            _ => ToolUseId::new(self.call_id.clone()).and_then(|id| {
                tool_request(id, operation, Value::Object(fields))
                    .and_then(|request| tools_outcome(vec![request]))
            }),
        }
        .map_err(|_| ResponseError::Outcome)
    }
}

impl WireResponse {
    fn completed_function(
        self,
        model: &str,
    ) -> Result<(String, Option<WireUsage>, FunctionCall), ResponseError> {
        self.validate_envelope(model)?;
        let mut call = None;
        for item in &self.output {
            match item.kind.as_str() {
                "reasoning" => {}
                "function_call"
                    if call.is_none() && item.status.as_deref() == Some("completed") =>
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
                    call = Some(FunctionCall {
                        name: item.name.clone().ok_or(ResponseError::Outcome)?,
                        arguments: item.arguments.clone().ok_or(ResponseError::Outcome)?,
                        call_id: item.call_id.clone().unwrap_or_default(),
                    });
                }
                _ => return Err(ResponseError::Message),
            }
        }
        let call = call.ok_or(ResponseError::MissingFinalMessage)?;
        call.operation()?;
        Ok((self.id, self.usage, call))
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
struct SummaryEnvelope {
    summary: String,
}

pub(super) fn is_structured_text(text: &str) -> bool {
    structured_outcome(text).is_ok() || serde_json::from_str::<SummaryEnvelope>(text).is_ok()
}

#[cfg(test)]
mod tests;

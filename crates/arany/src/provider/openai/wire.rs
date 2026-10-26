use crate::provider::compaction_input;
use crate::provider::conversation::{Block, Conversation, Role};
use crate::provider::decode::{argument_limit, function_outcome, review_outcome, tool_function};
use crate::provider::prompts::COMPACTION_INSTRUCTIONS;
use crate::provider::tool_specs::{NAME_PREFIX, ToolSpec, outcome_specs};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, ProviderError, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, ProviderWireProvenance, ToolUseId, Usage,
};
use crate::tools::ToolContext;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const ADAPTER: &str = "openai";
const MAX_REQUEST_BYTES: usize = crate::provider::MAX_PROVIDER_REQUEST_BYTES;
const DELEGATED_OUTPUT: &str = "Arany recorded this call; its result follows in a later message.";

pub(crate) fn encode_request(request: &Value) -> Result<Vec<u8>, ProviderError> {
    encode_request_within(request, MAX_REQUEST_BYTES)
}

pub(crate) fn encode_request_within(
    request: &Value,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::Rejected)?;
    if body.len() > max_bytes {
        return Err(ProviderError::Rejected);
    }
    Ok(body)
}

pub(crate) struct RunScope<'a> {
    pub(crate) model: &'a str,
    pub(crate) output_cap: u32,
    pub(crate) secret: &'a str,
    functions: Vec<String>,
    role: crate::provider::PromptRole,
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
            functions: allowed_functions(
                request,
                conversation.tools.as_deref().unwrap_or_default(),
                &conversation.outcome,
            ),
            role: request.input.role,
        }
    }
}

pub(super) fn allowed_functions(
    request: &ProviderRequest,
    tools: &[ToolSpec],
    outcome: &Value,
) -> Vec<String> {
    let exhausted = request
        .input
        .tools
        .as_ref()
        .is_some_and(ToolContext::answer_only);
    tools
        .iter()
        .filter(|_| !exhausted)
        .map(|spec| spec.name.clone())
        .chain(
            outcome_specs(outcome)
                .into_iter()
                .filter(|_| !exhausted)
                .map(|spec| spec.name),
        )
        .collect()
}

pub(crate) fn decode_run(
    bytes: &[u8],
    scope: &RunScope,
) -> Result<ProviderResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    decode(response, &raw["output"], scope).map_err(ResponseError::provider_error)
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
    let (response_id, usage, outcome) = if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        if scope.role == crate::provider::PromptRole::ToolReview {
            return Err(ResponseError::Outcome);
        }
        let (id, usage, calls) = response.completed_functions(scope.model)?;
        if scope.role == crate::provider::PromptRole::Editor && calls.len() != 1 {
            return Err(ResponseError::Outcome);
        }
        (id, usage, function_outcomes(&calls, &scope.functions)?)
    } else {
        let (id, usage, text) = response.completed_text(scope.model)?;
        let outcome = if scope.role == crate::provider::PromptRole::ToolReview {
            review_outcome(&text).map_err(|_| ResponseError::Outcome)?
        } else {
            ProviderOutcome::Answer(crate::provider::Answer { text })
        };
        (id, usage, outcome)
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
) -> crate::provider::diagnostics::SubscriptionFailureStage {
    outcome_failure_diagnostic(bytes, model).0
}

pub(crate) fn outcome_failure_diagnostic(
    bytes: &[u8],
    model: &str,
) -> (
    crate::provider::diagnostics::SubscriptionFailureStage,
    Option<crate::diagnostics::ToolArgumentShape>,
) {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    let Ok(response) = decode_completed_event(bytes) else {
        return (Stage::OutcomeContract, None);
    };
    if response.output.iter().any(|item| {
        let operation = item
            .name
            .as_deref()
            .and_then(|name| name.strip_prefix(NAME_PREFIX))
            .unwrap_or_default();
        item.kind == "function_call"
            && operation != "delegate"
            && item
                .arguments
                .as_ref()
                .is_some_and(|arguments| arguments.len() > argument_limit(operation))
    }) {
        return (Stage::OutcomeToolArgumentSize, None);
    }
    if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        let Ok((_, _, calls)) = response.completed_functions(model) else {
            return (Stage::OutcomeContract, None);
        };
        if let [call] = calls.as_slice() {
            return function_failure_diagnostic(call);
        }
        return calls
            .iter()
            .map(function_failure_diagnostic)
            .find(|(stage, _)| *stage != Stage::OutcomeContract)
            .unwrap_or((Stage::OutcomeContract, None));
    }
    let Ok((_, _, text)) = response.completed_text(model) else {
        return (Stage::OutcomeContract, None);
    };
    match serde_json::from_str::<Value>(&text) {
        Err(_) => (Stage::OutcomeEncoding, None),
        Ok(_) if review_outcome(&text).is_err() => (Stage::OutcomeFields, None),
        Ok(_) => (Stage::OutcomeContract, None),
    }
}

fn function_failure_diagnostic(
    call: &FunctionCall,
) -> (
    crate::provider::diagnostics::SubscriptionFailureStage,
    Option<crate::diagnostics::ToolArgumentShape>,
) {
    use crate::diagnostics::ToolArgumentShape;
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    let Some(operation) = call.name.strip_prefix(NAME_PREFIX) else {
        return (Stage::OutcomeContract, None);
    };
    let Ok(mut fields) = call.fields() else {
        return (Stage::OutcomeContract, None);
    };
    if operation == "plan" {
        return (Stage::OutcomeFields, None);
    }
    if matches!(operation, "delegate") {
        return match function_outcome(operation, fields) {
            Err(_) => (Stage::OutcomeFields, None),
            Ok(_) => (Stage::OutcomeContract, None),
        };
    }
    fields.insert("operation".into(), json!(operation));
    match serde_json::from_value::<crate::tools::ToolCall>(Value::Object(fields)) {
        Err(_) => (Stage::OutcomeFields, None),
        Ok(call) => {
            let stage = match &call {
                crate::tools::ToolCall::Read { offset, limit, .. }
                    if offset.is_some_and(|n| n == 0)
                        || limit.is_some_and(|n| {
                            !(1..=crate::tools::types::MAX_READ_PAGE_LINES).contains(&n)
                        }) =>
                {
                    Stage::OutcomeReadBounds
                }
                _ if !call.valid() => tool_failure_stage(&call),
                _ => Stage::OutcomeContract,
            };
            (stage, Some(ToolArgumentShape::from_call(&call)))
        }
    }
}

fn tool_failure_stage(
    call: &crate::tools::ToolCall,
) -> crate::provider::diagnostics::SubscriptionFailureStage {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    use crate::tools::{ToolCall, types::MAX_EDIT_CONTENT_BYTES};
    use crate::validate::{valid_digest, valid_name, valid_relative};
    if serde_json::to_vec(call).is_ok_and(|bytes| bytes.len() > call.argument_limit()) {
        return Stage::OutcomeToolArgumentSize;
    }
    if call
        .path_argument()
        .is_some_and(|(path, root)| !valid_relative(path, root))
    {
        return Stage::OutcomeToolPath;
    }
    match call {
        ToolCall::Move {
            source,
            destination,
            ..
        } if !valid_relative(source, false) || !valid_relative(destination, false) => {
            Stage::OutcomeToolPath
        }
        ToolCall::Delete {
            expected_digest, ..
        }
        | ToolCall::Move {
            expected_digest, ..
        } if !valid_digest(expected_digest) => Stage::OutcomeToolDigest,
        ToolCall::Edit {
            expected_digest, ..
        }
        | ToolCall::MultiEdit {
            expected_digest, ..
        } if !valid_digest(expected_digest) => Stage::OutcomeToolDigest,
        ToolCall::Write {
            expected_digest: Some(digest),
            ..
        } if !valid_digest(digest) => Stage::OutcomeToolDigest,
        ToolCall::Edit { old, .. } if old.is_empty() => Stage::OutcomeToolEmptyEdit,
        ToolCall::Edit { old, new, .. } if old.len() + new.len() > MAX_EDIT_CONTENT_BYTES => {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Write { content, .. } if content.len() > MAX_EDIT_CONTENT_BYTES => {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Search { query, include, .. }
            if query.is_empty()
                || query.len() > 512
                || include
                    .as_ref()
                    .is_some_and(|glob| glob.is_empty() || glob.len() > 512) =>
        {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Glob { pattern, .. } if pattern.is_empty() || pattern.len() > 512 => {
            Stage::OutcomeToolTextSize
        }
        ToolCall::Command { program, .. } | ToolCall::CommandUnguarded { program, .. }
            if !valid_name(program) =>
        {
            Stage::OutcomeToolProgram
        }
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
    if text.trim().is_empty() || text.len() > crate::session::MAX_COMPACTION_SUMMARY_BYTES {
        return Err(ResponseError::Outcome);
    }
    Ok(CompactionResponse {
        summary: text,
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
    );
    body["prompt_cache_key"] = json!(request.session_id.to_string());
    if request.input.role != crate::provider::PromptRole::ToolReview {
        let tools = conversation.tools.as_deref().unwrap_or_default();
        let functions: Vec<Value> = tools
            .iter()
            .cloned()
            .chain(outcome_specs(&crate::provider::run_outcome_schema(request)))
            .map(function_json)
            .collect();
        if !functions.is_empty() {
            body["tools"] = Value::Array(functions);
            let allowed = allowed_functions(request, tools, &conversation.outcome);
            body["tool_choice"] = if allowed.is_empty() {
                json!("none")
            } else {
                json!({
                    "type": "allowed_tools", "mode": "auto",
                    "tools": allowed.into_iter().map(|name| json!({"type":"function", "name":name})).collect::<Vec<_>>()
                })
            };
            body["parallel_tool_calls"] =
                json!(request.input.role != crate::provider::PromptRole::Editor);
        }
        body["include"] = json!(["reasoning.encrypted_content"]);
    } else {
        body["text"] = json!({"format": {"type": "json_schema", "name": "arany_outcome", "strict": true, "schema": conversation.outcome}});
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
                    .filter(|item| {
                        item["type"] == "function_call" && item["name"] == "arany_delegate"
                    })
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
    let mut body = response_body(
        &request.model,
        COMPACTION_INSTRUCTIONS,
        json!(input.to_string()),
        request.max_output_tokens,
        effort,
    );
    body["prompt_cache_key"] = json!(request.session_id.to_string());
    body
}

fn response_body(
    model: &str,
    instructions: &str,
    input: Value,
    max_output_tokens: u32,
    effort: Option<Effort>,
) -> Value {
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "max_output_tokens": max_output_tokens,
        "store": false,
        "truncation": "disabled",
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

// Input tokens already include cache subsets; do not add them again.
#[derive(Deserialize)]
struct WireInputTokensDetails {
    cached_tokens: Option<u32>,
    cache_write_tokens: Option<u32>,
}

fn provider_usage(usage: Option<&WireUsage>) -> Usage {
    Usage {
        input_tokens: usage.map(|value| value.input_tokens),
        output_tokens: usage.map(|value| value.output_tokens),
        cache_read_input_tokens: usage
            .and_then(|value| value.input_tokens_details.as_ref())
            .and_then(|details| details.cached_tokens),
        cache_creation_input_tokens: usage
            .and_then(|value| value.input_tokens_details.as_ref())
            .and_then(|details| details.cache_write_tokens),
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

// Keep raw completed items so replay preserves opaque reasoning ciphertext.
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
        let limit = argument_limit(self.operation()?);
        if self.arguments.len() > limit {
            return Err(ResponseError::Outcome);
        }
        let arguments = self.arguments.trim();
        match crate::tools::parse_json(arguments.as_bytes(), limit) {
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
            "delegate" => function_outcome(operation, fields),
            _ => ToolUseId::new(self.call_id.clone())
                .and_then(|id| tool_function(id, operation, Value::Object(fields))),
        }
        .map_err(|_| ResponseError::Outcome)
    }
}

fn function_outcomes(
    calls: &[FunctionCall],
    offered: &[String],
) -> Result<ProviderOutcome, ResponseError> {
    if let [call] = calls {
        return call.outcome(offered);
    }
    let mut requests = Vec::with_capacity(calls.len());
    for call in calls {
        if !offered.contains(&call.name) || matches!(call.operation()?, "plan" | "delegate") {
            return Err(ResponseError::Outcome);
        }
        requests.push(
            crate::provider::decode::tool_request(
                ToolUseId::new(call.call_id.clone()).map_err(|_| ResponseError::Outcome)?,
                call.operation()?,
                Value::Object(call.fields()?),
            )
            .map_err(|_| ResponseError::Outcome)?,
        );
    }
    crate::provider::decode::tools_outcome(requests).map_err(|_| ResponseError::Outcome)
}

impl WireResponse {
    fn completed_functions(
        self,
        model: &str,
    ) -> Result<(String, Option<WireUsage>, Vec<FunctionCall>), ResponseError> {
        self.validate_envelope(model)?;
        let mut calls = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for item in &self.output {
            match item.kind.as_str() {
                "reasoning" => {}
                "function_call" if item.status.as_deref() == Some("completed") => {
                    if item.content.is_some()
                        || item.phase.is_some()
                        || item.role.is_some()
                        || !item.completed_id().is_some_and(|id| ids.insert(id))
                        || item.call_id.as_deref().is_none_or(|id| {
                            id.is_empty()
                                || id.len() > 128
                                || !id.bytes().all(|byte| byte.is_ascii_graphic())
                        })
                    {
                        return Err(ResponseError::Message);
                    }
                    calls.push(FunctionCall {
                        name: item.name.clone().ok_or(ResponseError::Outcome)?,
                        arguments: item.arguments.clone().ok_or(ResponseError::Outcome)?,
                        call_id: item.call_id.clone().unwrap_or_default(),
                    });
                }
                "message" if calls.is_empty() => {
                    if item.role.as_deref() != Some("assistant")
                        || item
                            .status
                            .as_deref()
                            .is_some_and(|status| status != "completed")
                        || !matches!(item.phase.as_deref(), None | Some("commentary"))
                        || !item.content.as_ref().is_some_and(|parts| {
                            !parts.is_empty()
                                && parts
                                    .iter()
                                    .all(|part| part.kind == "output_text" && part.text.is_some())
                        })
                    {
                        return Err(ResponseError::Message);
                    }
                }
                _ => return Err(ResponseError::Message),
            }
        }
        if calls.is_empty() {
            return Err(ResponseError::MissingFinalMessage);
        }
        Ok((self.id, self.usage, calls))
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

#[cfg(test)]
mod tests;

use crate::provider::compaction_input;
use crate::provider::conversation::{Block, Conversation, Role};
use crate::provider::decode::{argument_limit, review_outcome, tool_function};
use crate::provider::prompts::COMPACTION_INSTRUCTIONS;
use crate::provider::tool_specs::{NAME_PREFIX, ToolSpec, outcome_specs};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, ProviderTurn,
    ProviderWireProvenance, ToolUseId, Usage,
};
use crate::tools::ToolContext;
use serde::Deserialize;
use serde_json::{Value, json};

const ADAPTER: &str = "anthropic";
const MAX_CONTENT_BLOCKS: usize = 16;
const DELEGATED_OUTPUT: &str = "Arany recorded this call; its result follows in a later message.";

pub(in crate::provider) struct MessageReply {
    pub(in crate::provider) bytes: Vec<u8>,
    pub(in crate::provider) counts: crate::provider::diagnostics::StreamCounts,
}

pub(super) struct MessageStream<'a> {
    framer: crate::provider::streaming::SseFramer,
    secret: &'a str,
    model: &'a str,
    preview: crate::provider::ResponsePreview,
    result: crate::provider::streaming::TextDecoder,
    response: Option<Value>,
    closed: Vec<bool>,
    delta_seen: bool,
    stopped: bool,
    tool_input: String,
    preview_failed: bool,
    stage: crate::provider::diagnostics::SubscriptionFailureStage,
    http_status: Option<u16>,
}

impl<'a> MessageStream<'a> {
    pub(super) fn end_error(&self) -> ProviderError {
        self.framer.end_error()
    }

    pub(super) fn new(
        secret: &'a str,
        model: &'a str,
        preview: crate::provider::ResponsePreview,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            framer: crate::provider::streaming::SseFramer::new(max_output_tokens),
            secret,
            model,
            preview,
            result: crate::provider::streaming::TextDecoder::new(),
            response: None,
            closed: Vec::new(),
            delta_seen: false,
            stopped: false,
            tool_input: String::new(),
            preview_failed: false,
            stage: crate::provider::diagnostics::SubscriptionFailureStage::EventFraming,
            http_status: None,
        }
    }

    pub(super) fn set_http_status(&mut self, status: u16) {
        self.http_status = Some(status);
    }

    #[cfg(test)]
    pub(super) fn stage(&self) -> crate::provider::diagnostics::SubscriptionFailureStage {
        self.stage
    }

    pub(super) fn counts(&self) -> crate::provider::diagnostics::StreamCounts {
        crate::provider::diagnostics::StreamCounts {
            bytes: self.framer.bytes,
            events: self.framer.events,
            http_status: self.http_status,
            ..Default::default()
        }
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        let result = (|| {
            self.stage = framing_stage(self.framer.stage);
            if let Err(error) = self.framer.admit(chunk.len()) {
                self.stage = framing_stage(self.framer.stage);
                return Err(error);
            }
            for &byte in chunk {
                self.stage = framing_stage(self.framer.stage);
                match self.framer.byte(byte, self.secret) {
                    Ok(Some(event)) => {
                        if let Some(response) = self.event(event.value)? {
                            return Ok(Some(response));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.stage = framing_stage(self.framer.stage);
                        return Err(error);
                    }
                }
            }
            Ok(None)
        })();
        if result.is_err() {
            crate::diagnostics::messages_failure(self.stage, self.counts());
        }
        result
    }

    fn publish_preview(&mut self, fragment: &str) {
        if self.preview_failed {
            return;
        }
        self.preview_failed = match self.result.feed(fragment, self.secret) {
            Ok(visible) => self
                .preview
                .publish(crate::provider::PreviewText {
                    answer: visible.to_owned(),
                    tentative: true,
                    ..Default::default()
                })
                .is_err(),
            Err(_) => true,
        };
    }

    fn event(&mut self, event: Value) -> Result<Option<Vec<u8>>, ProviderError> {
        self.stage = crate::provider::diagnostics::SubscriptionFailureStage::EventType;
        if self.stopped {
            return Err(ProviderError::InvalidStream);
        }
        let invalid = || ProviderError::InvalidStream;
        match event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?
        {
            "ping" => {}
            "error" => {
                self.stage = crate::provider::diagnostics::SubscriptionFailureStage::RemoteFailure;
                return Err(ProviderError::Unavailable);
            }
            "message_start" => {
                self.stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseMessage;
                let message = event.get("message").ok_or_else(invalid)?;
                if self.response.is_some()
                    || message.get("type").and_then(Value::as_str) != Some("message")
                    || message.get("role").and_then(Value::as_str) != Some("assistant")
                    || message.get("model").and_then(Value::as_str) != Some(self.model)
                    || message
                        .get("content")
                        .and_then(Value::as_array)
                        .is_none_or(|content| !content.is_empty())
                    || message
                        .get("stop_reason")
                        .is_none_or(|reason| !reason.is_null())
                {
                    return Err(ProviderError::InvalidStream);
                }
                self.response = Some(message.clone());
            }
            "content_block_start" => {
                self.stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(invalid)?;
                if index != self.closed.len()
                    || index >= MAX_CONTENT_BLOCKS
                    || self.closed.iter().any(|closed| !closed)
                    || self.delta_seen
                {
                    return Err(ProviderError::InvalidStream);
                }
                let block = event.get("content_block").ok_or_else(invalid)?;
                let content = self
                    .response
                    .as_mut()
                    .and_then(|message| message.get_mut("content"))
                    .and_then(Value::as_array_mut)
                    .ok_or_else(invalid)?;
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("input").is_some_and(|input| {
                        input.as_object().is_none_or(|fields| !fields.is_empty())
                    })
                {
                    return Err(ProviderError::InvalidStream);
                }
                content.push(block.clone());
                self.closed.push(false);
                self.tool_input.clear();
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    let initial = block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid)?;
                    self.publish_preview(initial);
                }
            }
            "content_block_delta" => {
                self.stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(invalid)?;
                if index.checked_add(1) != Some(self.closed.len())
                    || self.closed.get(index) != Some(&false)
                    || self.delta_seen
                {
                    return Err(ProviderError::InvalidStream);
                }
                let delta = event.get("delta").ok_or_else(invalid)?;
                let kind = delta
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                let block = self
                    .response
                    .as_mut()
                    .and_then(|message| message.get_mut("content"))
                    .and_then(Value::as_array_mut)
                    .and_then(|content| content.get_mut(index))
                    .ok_or_else(invalid)?;
                if kind == "input_json_delta" {
                    let fragment = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid)?;
                    if block.get("type").and_then(Value::as_str) != Some("tool_use")
                        || self.tool_input.len() + fragment.len() > tool_argument_limit(block)
                    {
                        return Err(ProviderError::InvalidStream);
                    }
                    self.tool_input.push_str(fragment);
                    return Ok(None);
                }
                let (field, expected) = match kind {
                    "text_delta" => ("text", "text"),
                    "thinking_delta" => ("thinking", "thinking"),
                    "signature_delta" => ("signature", "thinking"),
                    _ => return Err(ProviderError::InvalidStream),
                };
                if block.get("type").and_then(Value::as_str) != Some(expected) {
                    return Err(ProviderError::InvalidStream);
                }
                let fragment = delta
                    .get(field)
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                if field == "signature" && block.get(field).is_none() {
                    block[field] = Value::String(String::new());
                }
                let Some(Value::String(current)) = block.get_mut(field) else {
                    return Err(ProviderError::InvalidStream);
                };
                if current.len() + fragment.len() > MAX_RESPONSE_BYTES {
                    return Err(ProviderError::InvalidStream);
                }
                current.push_str(fragment);
                if kind == "text_delta" {
                    self.publish_preview(fragment);
                }
            }
            "content_block_stop" => {
                self.stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(invalid)?;
                let count = self.closed.len();
                let closed = self.closed.get_mut(index).ok_or_else(invalid)?;
                if *closed || index.checked_add(1) != Some(count) {
                    return Err(ProviderError::InvalidStream);
                }
                *closed = true;
                let block = self
                    .response
                    .as_mut()
                    .and_then(|message| message.get_mut("content"))
                    .and_then(Value::as_array_mut)
                    .and_then(|content| content.get_mut(index))
                    .ok_or_else(invalid)?;
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let input = if self.tool_input.is_empty() {
                        json!({})
                    } else {
                        crate::tools::parse_json(
                            self.tool_input.as_bytes(),
                            tool_argument_limit(block),
                        )
                        .map_err(|_| ProviderError::InvalidStream)?
                    };
                    if !input.is_object() {
                        return Err(ProviderError::InvalidStream);
                    }
                    block["input"] = input;
                    self.tool_input.clear();
                }
            }
            "message_delta" => {
                self.stage = crate::provider::diagnostics::SubscriptionFailureStage::UsageContract;
                if self.closed.is_empty() || self.closed.iter().any(|closed| !closed) {
                    return Err(ProviderError::InvalidStream);
                }
                let message = self.response.as_mut().ok_or_else(invalid)?;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(invalid)?;
                for (key, value) in delta {
                    if matches!(
                        key.as_str(),
                        "stop_reason" | "stop_sequence" | "stop_details"
                    ) {
                        message[key] = value.clone();
                    } else if matches!(key.as_str(), "id" | "model" | "role" | "type" | "content") {
                        return Err(ProviderError::InvalidStream);
                    }
                }
                let usage = event
                    .get("usage")
                    .and_then(Value::as_object)
                    .ok_or_else(invalid)?;
                let target = message
                    .get_mut("usage")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(invalid)?;
                for (key, value) in usage {
                    if key == "output_tokens"
                        && target
                            .get(key)
                            .and_then(Value::as_u64)
                            .zip(value.as_u64())
                            .is_some_and(|(before, after)| after < before)
                    {
                        return Err(ProviderError::InvalidStream);
                    }
                    target.insert(key.clone(), value.clone());
                }
                if !self.preview_failed {
                    let text = self.result.feed("", self.secret)?.to_owned();
                    let preview = if message["stop_reason"] == "tool_use" {
                        crate::provider::PreviewText {
                            commentary: if text.is_empty() { vec![] } else { vec![text] },
                            ..Default::default()
                        }
                    } else {
                        crate::provider::PreviewText {
                            answer: text,
                            ..Default::default()
                        }
                    };
                    self.preview.publish(preview)?;
                }
                self.delta_seen = true;
            }
            "message_stop" => {
                self.stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::CompletedResponse;
                if !self.delta_seen || self.closed.iter().any(|closed| !closed) {
                    return Err(ProviderError::InvalidStream);
                }
                let response = self.response.take().ok_or_else(invalid)?;
                if crate::provider::streaming::reflects_secret(&response, self.secret) {
                    self.stage = crate::provider::diagnostics::SubscriptionFailureStage::CredentialReflection;
                    return Err(ProviderError::InvalidOutcome);
                }
                let response =
                    serde_json::to_vec(&response).map_err(|_| ProviderError::InvalidStream)?;
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err(ProviderError::InvalidStream);
                }
                self.stopped = true;
                return Ok(Some(response));
            }
            _ => {}
        }
        Ok(None)
    }
}

fn framing_stage(
    stage: crate::provider::streaming::FramingStage,
) -> crate::provider::diagnostics::SubscriptionFailureStage {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    match stage {
        crate::provider::streaming::FramingStage::Limit => Stage::StreamLimit,
        crate::provider::streaming::FramingStage::Framing => Stage::EventFraming,
        crate::provider::streaming::FramingStage::Json => Stage::EventJson,
        crate::provider::streaming::FramingStage::Credential => Stage::CredentialReflection,
        crate::provider::streaming::FramingStage::Type => Stage::EventType,
    }
}

#[cfg(test)]
pub(in crate::provider) fn encode_request(request: &Value) -> Result<Vec<u8>, ProviderError> {
    encode_request_within(request, crate::provider::MAX_PROVIDER_REQUEST_BYTES)
}

pub(in crate::provider) fn encode_request_within(
    request: &Value,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::Rejected)?;
    if body.len() > max_bytes {
        return Err(ProviderError::Rejected);
    }
    Ok(body)
}

pub(in crate::provider) struct RunScope<'a> {
    pub(in crate::provider) model: &'a str,
    pub(in crate::provider) output_cap: u32,
    pub(in crate::provider) secret: &'a str,
    pub(in crate::provider) offered: &'a [ToolSpec],
    pub(in crate::provider) review: bool,
}

fn tool_argument_limit(block: &Value) -> usize {
    argument_limit(
        block["name"]
            .as_str()
            .and_then(|name| name.strip_prefix(NAME_PREFIX))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
pub(in crate::provider) fn decode_run(
    bytes: &[u8],
    scope: &RunScope,
) -> Result<ProviderResponse, ProviderError> {
    decode_run_with_counts(
        bytes,
        scope,
        crate::provider::diagnostics::StreamCounts {
            bytes: bytes.len(),
            ..Default::default()
        },
    )
}

pub(in crate::provider) fn decode_run_with_counts(
    bytes: &[u8],
    scope: &RunScope,
    counts: crate::provider::diagnostics::StreamCounts,
) -> Result<ProviderResponse, ProviderError> {
    let (result, stage, tool_arguments) = decode_run_staged(bytes, scope);
    if result.is_err() {
        crate::diagnostics::messages_failure(
            stage,
            crate::provider::diagnostics::StreamCounts {
                tool_arguments,
                ..counts
            },
        );
    }
    result
}

fn decode_run_staged(
    bytes: &[u8],
    scope: &RunScope,
) -> (
    Result<ProviderResponse, ProviderError>,
    crate::provider::diagnostics::SubscriptionFailureStage,
    Option<crate::diagnostics::ToolArgumentShape>,
) {
    let mut stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope;
    let mut tool_arguments = None;
    let result = (|| {
        let response: WireResponse =
            serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
        stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
        let admitted = response.admit(scope.model, scope.output_cap, &mut stage)?;
        let (outcome, provenance) = match admitted.stop {
            Stop::EndTurn => (
                if scope.review {
                    review_outcome(&admitted.single_text()?)?
                } else {
                    ProviderOutcome::Answer(crate::provider::Answer {
                        text: admitted.answer_text()?,
                    })
                },
                ProviderWireProvenance::MessagesEndTurnStorageUnspecified,
            ),
            Stop::ToolUse => {
                stage =
                    crate::provider::diagnostics::SubscriptionFailureStage::OutcomeToolArguments;
                match admitted.tool_requests(scope.offered) {
                    Ok(outcome) => (
                        outcome,
                        ProviderWireProvenance::MessagesToolUseStorageUnspecified,
                    ),
                    Err(failure) => {
                        tool_arguments = failure.tool_arguments.map(|shape| *shape);
                        return Err(failure.error);
                    }
                }
            }
        };
        stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
        let raw: Value =
            serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
        let turn = ProviderTurn::new(
            ADAPTER,
            &raw["content"].to_string(),
            scope.secret,
            scope.output_cap,
        )?;
        Ok(ProviderResponse {
            outcome,
            turn,
            response_id: Some(admitted.id),
            usage: admitted.usage,
            wire_provenance: Some(provenance),
        })
    })();
    (result, stage, tool_arguments)
}

#[cfg(test)]
pub(super) fn decode_run_failure_stage(
    bytes: &[u8],
    scope: &RunScope,
) -> crate::provider::diagnostics::SubscriptionFailureStage {
    decode_run_staged(bytes, scope).1
}

#[cfg(test)]
pub(super) fn decode_run_failure_tool_shape(
    bytes: &[u8],
    scope: &RunScope,
) -> Option<crate::diagnostics::ToolArgumentShape> {
    decode_run_staged(bytes, scope).2
}

pub(in crate::provider) fn decode_compaction_with_counts(
    bytes: &[u8],
    model: &str,
    output_cap: u32,
    counts: crate::provider::diagnostics::StreamCounts,
) -> Result<CompactionResponse, ProviderError> {
    decode_compaction_observed(bytes, model, output_cap, counts)
}

#[cfg(test)]
pub(in crate::provider) fn decode_compaction(
    bytes: &[u8],
    model: &str,
    output_cap: u32,
) -> Result<CompactionResponse, ProviderError> {
    decode_compaction_observed(
        bytes,
        model,
        output_cap,
        crate::provider::diagnostics::StreamCounts {
            bytes: bytes.len(),
            ..Default::default()
        },
    )
}

fn decode_compaction_observed(
    bytes: &[u8],
    model: &str,
    output_cap: u32,
    counts: crate::provider::diagnostics::StreamCounts,
) -> Result<CompactionResponse, ProviderError> {
    let mut stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseEnvelope;
    let result = (|| {
        let response: WireResponse =
            serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
        stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
        let admitted = response.admit(model, output_cap, &mut stage)?;
        if admitted.stop != Stop::EndTurn {
            return Err(ProviderError::InvalidOutcome);
        }
        let summary = admitted.single_text()?;
        if summary.trim().is_empty() || summary.len() > crate::session::MAX_COMPACTION_SUMMARY_BYTES
        {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(CompactionResponse {
            summary,
            response_id: Some(admitted.id),
            usage: admitted.usage,
            wire_provenance: Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified),
        })
    })();
    if result.is_err() {
        crate::diagnostics::messages_failure(
            stage,
            crate::provider::diagnostics::StreamCounts { ..counts },
        );
    }
    result
}

// Put stable Tools and system blocks first so later requests preserve the earlier prefix.
pub(crate) fn run_body(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Effort,
) -> Result<Value, ProviderError> {
    run_body_with_optional_effort(request, conversation, Some(effort))
}

pub(in crate::provider) fn run_body_with_optional_effort(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Option<Effort>,
) -> Result<Value, ProviderError> {
    let system: Vec<Value> = conversation
        .system
        .iter()
        .map(|block| json!({"type": "text", "text": block.text()}))
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "system": system,
        "messages": messages(conversation)?,
        "output_config": {"format": {"type": "json_schema", "schema": conversation.outcome}, "effort": effort.map(Effort::as_str)},
        "stream": false,
    });
    if request.input.role != crate::provider::PromptRole::ToolReview {
        body["output_config"]
            .as_object_mut()
            .expect("output config")
            .remove("format");
    }
    if effort.is_none() {
        body["output_config"]
            .as_object_mut()
            .expect("output config")
            .remove("effort");
        if body["output_config"]
            .as_object()
            .is_some_and(|config| config.is_empty())
        {
            body.as_object_mut()
                .expect("request object")
                .remove("output_config");
        }
    }
    // Plan-contract unions exceed strict Tool-schema limits; the Engine validates calls instead.
    let exhausted = request
        .input
        .tools
        .as_ref()
        .is_some_and(ToolContext::answer_only);
    if request.input.role != crate::provider::PromptRole::ToolReview {
        let tools = conversation.tools.as_deref().unwrap_or_default();
        let specs: Vec<Value> = tools.iter().cloned().chain(outcome_specs(&conversation.outcome))
            .map(|tool| {
                let mut parameters = tool.parameters;
                if tool.name == "arany_read"
                    && let Some(required) = parameters["required"].as_array_mut()
                {
                    required.retain(|field| field != "offset" && field != "limit");
                    parameters["properties"]["offset"]["description"] = json!(format!("Optional 1-based start line, 1 through {}; omitted or null starts at line 1. Continue using next_line.", u32::MAX));
                    parameters["properties"]["limit"]["description"] = json!(format!("Optional number of lines, 1 through {}; omitted or null reads up to {} lines. Lines are numbered and cut after {} characters; the rendered result budget may end the page earlier.", crate::tools::types::MAX_READ_PAGE_LINES, crate::tools::types::MAX_READ_PAGE_LINES, crate::tools::types::MAX_READ_LINE_CHARS));
                }
                json!({"name": tool.name, "description": tool.description, "input_schema": parameters})
            })
            .collect();
        if !specs.is_empty() {
            body["tools"] = Value::Array(specs);
            body["tool_choice"] = if exhausted {
                json!({"type": "none"})
            } else {
                json!({"type": "auto", "disable_parallel_tool_use": true})
            };
        }
    }
    cache_breakpoints(&mut body, conversation.tools.is_some() && !exhausted);
    Ok(body)
}

fn cache_breakpoints(body: &mut Value, cache_tail: bool) {
    for key in ["tools", "system"] {
        if let Some(last) = body
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
        {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    if cache_tail
        && let Some(last) = body["messages"]
            .as_array_mut()
            .and_then(|messages| messages.last_mut())
            .and_then(|message| message["content"].as_array_mut())
            .and_then(|blocks| blocks.last_mut())
    {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
}

fn messages(conversation: &Conversation) -> Result<Vec<Value>, ProviderError> {
    let mut unanswered: Vec<String> = Vec::new();
    conversation
        .messages
        .iter()
        .map(|message| match message.role {
            Role::User => {
                let mut content = Vec::new();
                for id in std::mem::take(&mut unanswered) {
                    let answered = message.blocks.iter().any(
                        |block| matches!(block, Block::ToolResult { id: result, .. } if result.as_str() == id),
                    );
                    if !answered {
                        content.push(json!({"type": "tool_result", "tool_use_id": id, "content": DELEGATED_OUTPUT}));
                    }
                }
                for block in &message.blocks {
                    content.extend(user_blocks(block)?);
                }
                Ok(json!({"role": "user", "content": content}))
            }
            Role::Assistant => {
                let [Block::AssistantTurn(turn)] = message.blocks.as_slice() else {
                    return Err(ProviderError::Rejected);
                };
                let items = turn.items(ADAPTER).ok_or(ProviderError::Rejected)?;
                unanswered = items
                    .iter()
                    .filter(|item| item["type"] == "tool_use" && item["name"] == "arany_delegate")
                    .filter_map(|item| item["id"].as_str().map(str::to_owned))
                    .collect();
                Ok(json!({"role": "assistant", "content": items}))
            }
        })
        .collect()
}

fn user_blocks(block: &Block) -> Result<Vec<Value>, ProviderError> {
    Ok(match block {
        Block::Text(text) if text.is_empty() => Vec::new(),
        Block::Text(text) => vec![json!({"type": "text", "text": text})],
        Block::Image { label, image } => vec![
            json!({"type": "text", "text": label}),
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": image.base64()},
            }),
        ],
        Block::ToolResult {
            id,
            output,
            is_error,
        } => vec![json!({
            "type": "tool_result",
            "tool_use_id": id.as_str(),
            "content": output.to_string(),
            "is_error": is_error,
        })],
        Block::AssistantTurn(_) => return Err(ProviderError::Rejected),
    })
}

pub(in crate::provider) fn compaction_body(request: &CompactionRequest, effort: Effort) -> Value {
    compaction_body_with_optional_effort(request, Some(effort))
}

pub(in crate::provider) fn compaction_body_with_optional_effort(
    request: &CompactionRequest,
    effort: Option<Effort>,
) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "system": [{"type": "text", "text": COMPACTION_INSTRUCTIONS}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": compaction_input(request).to_string()}]}],
        "stream": false,
    });
    if let Some(effort) = effort {
        body["output_config"] = json!({"effort": effort.as_str()});
    }
    cache_breakpoints(&mut body, false);
    body
}

#[derive(Deserialize)]
struct WireResponse<'a> {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    model: String,
    stop_reason: String,
    stop_details: Option<Value>,
    stop_sequence: Option<String>,
    #[serde(borrow)]
    content: Vec<WireContent<'a>>,
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireContent<'a> {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    data: Option<String>,
    citations: Option<Value>,
    id: Option<String>,
    name: Option<String>,
    #[serde(borrow)]
    input: Option<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_creation: Option<WireCacheCreation>,
    cache_read_input_tokens: Option<u32>,
    output_tokens: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCacheCreation {
    ephemeral_5m_input_tokens: u32,
    ephemeral_1h_input_tokens: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    EndTurn,
    ToolUse,
}

struct Admitted<'a> {
    id: String,
    usage: Usage,
    stop: Stop,
    content: Vec<WireContent<'a>>,
}

struct ToolRequestFailure {
    error: ProviderError,
    tool_arguments: Option<Box<crate::diagnostics::ToolArgumentShape>>,
}

impl WireContent<'_> {
    fn is_thinking(&self) -> bool {
        match self.kind.as_str() {
            "thinking" => {
                self.text.is_none()
                    && self.data.is_none()
                    && self.citations.is_none()
                    && self.id.is_none()
                    && self.name.is_none()
                    && self.input.is_none()
            }
            "redacted_thinking" => {
                self.text.is_none()
                    && self.thinking.is_none()
                    && self.signature.is_none()
                    && self.citations.is_none()
                    && self.id.is_none()
                    && self.name.is_none()
                    && self.input.is_none()
            }
            _ => false,
        }
    }

    fn is_text(&self) -> bool {
        self.kind == "text"
            && self.text.is_some()
            && self.thinking.is_none()
            && self.signature.is_none()
            && self.data.is_none()
            && self.citations.is_none()
            && self.id.is_none()
            && self.name.is_none()
            && self.input.is_none()
    }

    fn is_tool_use(&self) -> bool {
        self.kind == "tool_use"
            && self.id.is_some()
            && self.name.is_some()
            && self.input.is_some()
            && self.text.is_none()
            && self.thinking.is_none()
            && self.signature.is_none()
            && self.data.is_none()
            && self.citations.is_none()
    }
}

impl<'a> WireResponse<'a> {
    fn admit(
        self,
        model: &str,
        output_cap: u32,
        stage: &mut crate::provider::diagnostics::SubscriptionFailureStage,
    ) -> Result<Admitted<'a>, ProviderError> {
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::UsageContract;
        let nested_creation = self
            .usage
            .cache_creation
            .map(|creation| {
                creation
                    .ephemeral_5m_input_tokens
                    .checked_add(creation.ephemeral_1h_input_tokens)
                    .ok_or(ProviderError::InvalidOutcome)
            })
            .transpose()?;
        if self
            .usage
            .cache_creation_input_tokens
            .zip(nested_creation)
            .is_some_and(|(flat, nested)| flat != nested)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let cache_creation_input_tokens =
            self.usage.cache_creation_input_tokens.or(nested_creation);
        // Messages reports cache counts beside `input_tokens`; the record total keeps the billed sum.
        let total_input_tokens = self
            .usage
            .input_tokens
            .checked_add(cache_creation_input_tokens.unwrap_or(0))
            .and_then(|total| total.checked_add(self.usage.cache_read_input_tokens.unwrap_or(0)))
            .ok_or(ProviderError::InvalidOutcome)?;
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseStatus;
        if self.stop_details.is_some() || self.stop_sequence.is_some() {
            return Err(ProviderError::InvalidOutcome);
        }
        let stop = match self.stop_reason.as_str() {
            "end_turn" => Stop::EndTurn,
            "tool_use" => Stop::ToolUse,
            _ => return Err(ProviderError::InvalidOutcome),
        };
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseModel;
        if self.kind != "message" || self.role != "assistant" || self.model != model {
            return Err(ProviderError::InvalidOutcome);
        }
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::UsageContract;
        if total_input_tokens > MAX_REPORTED_INPUT_TOKENS || self.usage.output_tokens > output_cap {
            return Err(ProviderError::InvalidOutcome);
        }
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseIdentifier;
        if self.id.is_empty()
            || self.id.len() > 128
            || !self.id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ProviderError::InvalidOutcome);
        }
        *stage = crate::provider::diagnostics::SubscriptionFailureStage::ResponseContent;
        if self.content.is_empty() || self.content.len() > MAX_CONTENT_BLOCKS {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(Admitted {
            id: self.id,
            usage: Usage {
                input_tokens: Some(total_input_tokens),
                output_tokens: Some(self.usage.output_tokens),
                cache_read_input_tokens: self.usage.cache_read_input_tokens,
                cache_creation_input_tokens,
            },
            stop,
            content: self.content,
        })
    }
}

impl Admitted<'_> {
    fn single_text(&self) -> Result<String, ProviderError> {
        let mut text = None;
        for block in &self.content {
            if block.is_text() && text.is_none() {
                text = block.text.clone();
            } else if !block.is_thinking() || text.is_some() {
                return Err(ProviderError::InvalidOutcome);
            }
        }
        text.ok_or(ProviderError::InvalidOutcome)
    }

    fn answer_text(&self) -> Result<String, ProviderError> {
        let mut text = String::new();
        for block in &self.content {
            if block.is_text() {
                text.push_str(block.text.as_deref().unwrap_or_default());
            } else if !block.is_thinking() || !text.is_empty() {
                return Err(ProviderError::InvalidOutcome);
            }
        }
        if text.trim().is_empty() {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(text)
    }

    fn tool_requests(&self, offered: &[ToolSpec]) -> Result<ProviderOutcome, ToolRequestFailure> {
        let mut requests = Vec::new();
        for block in &self.content {
            if block.is_tool_use() {
                let name = block.name.as_deref().unwrap_or_default();
                let operation = name
                    .strip_prefix(NAME_PREFIX)
                    .filter(|_| offered.iter().any(|spec| spec.name == name))
                    .ok_or(ToolRequestFailure {
                        error: ProviderError::InvalidOutcome,
                        tool_arguments: None,
                    })?;
                let raw_input = block.input.ok_or(ToolRequestFailure {
                    error: ProviderError::InvalidOutcome,
                    tool_arguments: None,
                })?;
                let input =
                    crate::tools::parse_json(raw_input.get().as_bytes(), argument_limit(operation))
                        .map_err(|_| ToolRequestFailure {
                            error: ProviderError::InvalidOutcome,
                            tool_arguments: diagnostic_tool_arguments(operation, raw_input)
                                .map(Box::new),
                        })?;
                let id = ToolUseId::new(block.id.clone().unwrap_or_default()).map_err(|error| {
                    ToolRequestFailure {
                        error,
                        tool_arguments: diagnostic_tool_arguments(operation, raw_input)
                            .map(Box::new),
                    }
                })?;
                requests.push(tool_function(id, operation, input).map_err(|error| {
                    ToolRequestFailure {
                        error,
                        tool_arguments: diagnostic_tool_arguments(operation, raw_input)
                            .map(Box::new),
                    }
                })?);
            } else if (!block.is_thinking() && !block.is_text()) || !requests.is_empty() {
                return Err(ToolRequestFailure {
                    error: ProviderError::InvalidOutcome,
                    tool_arguments: None,
                });
            }
        }
        match <[_; 1]>::try_from(requests) {
            Ok([outcome]) => Ok(outcome),
            Err(_) => Err(ToolRequestFailure {
                error: ProviderError::InvalidOutcome,
                tool_arguments: None,
            }),
        }
    }
}

fn diagnostic_tool_arguments(
    operation: &str,
    raw_input: &serde_json::value::RawValue,
) -> Option<crate::diagnostics::ToolArgumentShape> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let operation = crate::tools::catalog::by_name(operation)?.name;
    let call = crate::tools::parse_json(raw_input.get().as_bytes(), argument_limit(operation))
        .ok()
        .and_then(|input| {
            let Value::Object(mut fields) = input else {
                return None;
            };
            fields.insert("operation".into(), Value::String(operation.to_owned()));
            serde_json::from_value::<crate::tools::ToolCall>(Value::Object(fields)).ok()
        });
    Some(call.map_or_else(
        || crate::diagnostics::ToolArgumentShape::from_serialized(operation, raw_input.get().len()),
        |call| crate::diagnostics::ToolArgumentShape::from_call(&call),
    ))
}

#[cfg(test)]
mod tests;

use crate::provider::conversation::{Block, Conversation, Role};
use crate::provider::decode::{structured_outcome, tool_request, tools_outcome};
use crate::provider::prompts::COMPACTION_INSTRUCTIONS;
use crate::provider::tool_specs::{NAME_PREFIX, ToolSpec};
use crate::provider::{
    CompactionRequest, CompactionResponse, Effort, MAX_REPORTED_INPUT_TOKENS, MAX_RESPONSE_BYTES,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, ProviderTurn,
    ProviderWireProvenance, ToolUseId, Usage,
};
use crate::provider::{compaction_input, summary_schema};
use crate::tools::ToolContext;
use crate::tools::types::MAX_TOOL_ARGUMENT_BYTES;
use serde::Deserialize;
use serde_json::{Value, json};

const ADAPTER: &str = "anthropic";
const MAX_REQUEST_BYTES: usize = crate::provider::MAX_PROVIDER_REQUEST_BYTES;
const MAX_CONTENT_BLOCKS: usize = 16;

pub(super) struct MessageStream<'a> {
    framer: crate::provider::streaming::SseFramer,
    secret: &'a str,
    model: &'a str,
    preview: crate::provider::ResponsePreview,
    result: crate::provider::streaming::ResultDecoder,
    response: Option<Value>,
    closed: Vec<bool>,
    delta_seen: bool,
    stopped: bool,
    tool_input: String,
    preview_failed: bool,
}

impl<'a> MessageStream<'a> {
    pub(super) fn new(
        secret: &'a str,
        model: &'a str,
        preview: crate::provider::ResponsePreview,
    ) -> Self {
        Self {
            framer: crate::provider::streaming::SseFramer::new(),
            secret,
            model,
            preview,
            result: crate::provider::streaming::ResultDecoder::new(false),
            response: None,
            closed: Vec::new(),
            delta_seen: false,
            stopped: false,
            tool_input: String::new(),
            preview_failed: false,
        }
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        self.framer.admit(chunk.len())?;
        for &byte in chunk {
            if let Some(event) = self.framer.byte(byte, self.secret)?
                && let Some(response) = self.event(event.value)?
            {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    // Tool-adjacent text need not be the structured answer, so failed preview decoding stops only the preview.
    fn publish_preview(&mut self, fragment: &str) {
        if self.preview_failed {
            return;
        }
        let published = self
            .result
            .feed(fragment, self.secret)
            .map(str::to_owned)
            .and_then(|answer| {
                self.preview.publish(crate::provider::PreviewText {
                    answer,
                    ..Default::default()
                })
            });
        self.preview_failed = published.is_err();
    }

    fn event(&mut self, event: Value) -> Result<Option<Vec<u8>>, ProviderError> {
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
            "error" => return Err(ProviderError::Unavailable),
            "message_start" => {
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
                        || self.tool_input.len() + fragment.len() > MAX_TOOL_ARGUMENT_BYTES
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
                            MAX_TOOL_ARGUMENT_BYTES,
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
                self.delta_seen = true;
            }
            "message_stop" => {
                if !self.delta_seen || self.closed.iter().any(|closed| !closed) {
                    return Err(ProviderError::InvalidStream);
                }
                let response = self.response.take().ok_or_else(invalid)?;
                if crate::provider::streaming::reflects_secret(&response, self.secret) {
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

pub(super) fn encode_request(request: &Value) -> Result<Vec<u8>, ProviderError> {
    let body = serde_json::to_vec(request).map_err(|_| ProviderError::Rejected)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ProviderError::Rejected);
    }
    Ok(body)
}

pub(super) struct RunScope<'a> {
    pub(super) model: &'a str,
    pub(super) output_cap: u32,
    pub(super) secret: &'a str,
    pub(super) offered: &'a [ToolSpec],
}

pub(super) fn decode_run(
    bytes: &[u8],
    scope: &RunScope,
) -> Result<ProviderResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    let turn = ProviderTurn::new(
        ADAPTER,
        &raw["content"].to_string(),
        scope.secret,
        scope.output_cap,
    )?;
    let admitted = response.admit(scope.model, scope.output_cap)?;
    let (outcome, provenance) = match admitted.stop {
        Stop::EndTurn => (
            structured_outcome(&admitted.single_text()?)?,
            ProviderWireProvenance::MessagesEndTurnStorageUnspecified,
        ),
        Stop::ToolUse => (
            admitted.tool_requests(scope.offered)?,
            ProviderWireProvenance::MessagesToolUseStorageUnspecified,
        ),
    };
    Ok(ProviderResponse {
        outcome,
        turn,
        response_id: Some(admitted.id),
        usage: admitted.usage,
        wire_provenance: Some(provenance),
    })
}

pub(super) fn decode_compaction(
    bytes: &[u8],
    model: &str,
    output_cap: u32,
) -> Result<CompactionResponse, ProviderError> {
    let response: WireResponse =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidOutcome)?;
    let admitted = response.admit(model, output_cap)?;
    if admitted.stop != Stop::EndTurn {
        return Err(ProviderError::InvalidOutcome);
    }
    let summary: SummaryEnvelope = serde_json::from_str(&admitted.single_text()?)
        .map_err(|_| ProviderError::InvalidOutcome)?;
    Ok(CompactionResponse {
        summary: summary.summary,
        response_id: Some(admitted.id),
        usage: admitted.usage,
        wire_provenance: Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified),
    })
}

// Put stable Tools and system blocks first so later requests preserve the earlier prefix.
pub(crate) fn run_body(
    request: &ProviderRequest,
    conversation: &Conversation,
    effort: Effort,
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
        "output_config": {"format": {"type": "json_schema", "schema": conversation.outcome}, "effort": effort.as_str()},
        "stream": false,
    });
    // Plan-contract unions exceed strict Tool-schema limits; the Engine validates calls instead.
    let exhausted = request
        .input
        .tools
        .as_ref()
        .is_some_and(ToolContext::finish_only);
    if let Some(tools) = &conversation.tools {
        body["tools"] = tools
            .iter()
            .map(|tool| json!({"name": tool.name, "description": tool.description, "input_schema": tool.parameters}))
            .collect();
        body["tool_choice"] = if exhausted {
            json!({"type": "none"})
        } else {
            json!({"type": "auto", "disable_parallel_tool_use": true})
        };
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
    conversation
        .messages
        .iter()
        .map(|message| match message.role {
            Role::User => Ok(json!({
                "role": "user",
                "content": message.blocks.iter().map(user_blocks).collect::<Result<Vec<_>, _>>()?.concat(),
            })),
            Role::Assistant => {
                let [Block::AssistantTurn(turn)] = message.blocks.as_slice() else {
                    return Err(ProviderError::Rejected);
                };
                Ok(json!({
                    "role": "assistant",
                    "content": turn.items(ADAPTER).ok_or(ProviderError::Rejected)?,
                }))
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

pub(super) fn compaction_body(request: &CompactionRequest, effort: Effort) -> Value {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "system": [{"type": "text", "text": COMPACTION_INSTRUCTIONS}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": compaction_input(request).to_string()}]}],
        "output_config": {"format": {"type": "json_schema", "schema": summary_schema()}, "effort": effort.as_str()},
        "stream": false,
    });
    cache_breakpoints(&mut body, false);
    body
}

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    model: String,
    stop_reason: String,
    stop_details: Option<Value>,
    stop_sequence: Option<String>,
    content: Vec<WireContent>,
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    data: Option<String>,
    citations: Option<Value>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    output_tokens: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    EndTurn,
    ToolUse,
}

struct Admitted {
    id: String,
    usage: Usage,
    stop: Stop,
    content: Vec<WireContent>,
}

impl WireContent {
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

impl WireResponse {
    fn admit(self, model: &str, output_cap: u32) -> Result<Admitted, ProviderError> {
        // Messages reports cache counts beside `input_tokens`; the record total keeps the billed sum.
        let total_input_tokens = self
            .usage
            .input_tokens
            .checked_add(self.usage.cache_creation_input_tokens.unwrap_or(0))
            .and_then(|total| total.checked_add(self.usage.cache_read_input_tokens.unwrap_or(0)))
            .ok_or(ProviderError::InvalidOutcome)?;
        let stop = match self.stop_reason.as_str() {
            "end_turn" => Stop::EndTurn,
            "tool_use" => Stop::ToolUse,
            _ => return Err(ProviderError::InvalidOutcome),
        };
        if self.kind != "message"
            || self.role != "assistant"
            || self.stop_details.is_some()
            || self.stop_sequence.is_some()
            || self.model != model
            || self.id.is_empty()
            || self.id.len() > 128
            || !self.id.bytes().all(|byte| byte.is_ascii_graphic())
            || total_input_tokens > MAX_REPORTED_INPUT_TOKENS
            || self.usage.output_tokens > output_cap
            || self.content.is_empty()
            || self.content.len() > MAX_CONTENT_BLOCKS
        {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(Admitted {
            id: self.id,
            usage: Usage {
                input_tokens: Some(total_input_tokens),
                output_tokens: Some(self.usage.output_tokens),
                cache_read_input_tokens: self.usage.cache_read_input_tokens,
                cache_creation_input_tokens: self.usage.cache_creation_input_tokens,
            },
            stop,
            content: self.content,
        })
    }
}

impl Admitted {
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

    fn tool_requests(&self, offered: &[ToolSpec]) -> Result<ProviderOutcome, ProviderError> {
        let mut requests = Vec::new();
        for block in &self.content {
            if block.is_tool_use() {
                let name = block.name.as_deref().unwrap_or_default();
                let operation = name
                    .strip_prefix(NAME_PREFIX)
                    .filter(|_| offered.iter().any(|spec| spec.name == name))
                    .ok_or(ProviderError::InvalidOutcome)?;
                requests.push(tool_request(
                    ToolUseId::new(block.id.clone().unwrap_or_default())?,
                    operation,
                    block.input.clone().unwrap_or_default(),
                )?);
            } else if !block.is_thinking() && !block.is_text() {
                return Err(ProviderError::InvalidOutcome);
            }
        }
        tools_outcome(requests)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryEnvelope {
    summary: String,
}

#[cfg(test)]
mod tests;

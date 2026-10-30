use super::limits;
use super::wire;
use crate::provider::diagnostics::{
    ResponseShape, StreamCounts, SubscriptionFailureStage as Stage,
};
use crate::provider::streaming::{FramingStage, ResponsesPreview, SseEvent, SseFramer};
use crate::provider::{MAX_RESPONSE_BYTES, ProviderError, RateLimitSlot, ResponsePreview};
use serde::Deserialize;
use serde_json::Value;

const MAX_EVENTS: usize = 8192;

pub(super) async fn read_completed_response(
    response: &mut reqwest::Response,
    key: &str,
    model: &str,
    preview: Option<ResponsePreview>,
    max_output_tokens: u32,
) -> Result<Vec<u8>, ProviderError> {
    crate::provider::streaming::validate_stream_response(response, max_output_tokens).map_err(
        |rejection| {
            let counts = StreamCounts {
                http_status: Some(response.status().as_u16()),
                ..StreamCounts::default()
            };
            crate::provider::diagnostics::subscription_failure(rejection.stage, counts);
            rejection.error
        },
    )?;
    let mut decoder = StreamDecoder::with_preview(key, model, preview, max_output_tokens);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError::PartialTransport)?
    {
        if let Some(completed) = decoder.feed(&chunk)? {
            return Ok(completed);
        }
    }
    Err(decoder.end_error())
}

pub(super) struct StreamDecoder<'a> {
    limits: RateLimitSlot,
    token: &'a str,
    framer: SseFramer,
    preview: Option<ResponsesPreview<'a>>,
    stage: Stage,
    response_shape: ResponseShape,
    response_id: Option<String>,
    done_items: Vec<(wire::WireItem, Value)>,
    // Kept raw: only the ChatGPT completion path interprets it, so shared streams stay unchanged.
    output_tokens_details: Option<Value>,
}

#[derive(Deserialize)]
struct CreatedEvent {
    response: Option<CreatedResponse>,
}

#[derive(Deserialize)]
struct CreatedResponse {
    id: String,
}

#[derive(Deserialize)]
struct DoneEvent {
    output_index: usize,
    item: wire::WireItem,
}

impl<'a> StreamDecoder<'a> {
    pub(super) fn new(token: &'a str, max_output_tokens: u32) -> Self {
        Self {
            token,
            framer: SseFramer::new(max_output_tokens),
            limits: RateLimitSlot::default(),
            preview: None,
            stage: Stage::EventFraming,
            response_shape: ResponseShape::default(),
            response_id: None,
            done_items: Vec::new(),
            output_tokens_details: None,
        }
    }

    pub(super) fn with_preview(
        token: &'a str,
        model: &'a str,
        preview: Option<ResponsePreview>,
        max_output_tokens: u32,
    ) -> Self {
        let mut decoder = Self::new(token, max_output_tokens);
        decoder.preview = preview.map(|preview| ResponsesPreview::new(token, model, preview));
        decoder
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        self.framer.admit(chunk.len()).inspect_err(|_| {
            self.stage = Stage::StreamLimit;
        })?;
        for &byte in chunk {
            let event = self.framer.byte(byte, self.token).inspect_err(|_| {
                self.stage = match self.framer.stage {
                    FramingStage::Limit => Stage::StreamLimit,
                    FramingStage::Framing => Stage::EventFraming,
                    FramingStage::Json => Stage::EventJson,
                    FramingStage::Credential => Stage::CredentialReflection,
                    FramingStage::Type => Stage::EventType,
                };
            })?;
            if let Some(event) = event
                && let Some(completed) = self.finish_event(event)?
            {
                return Ok(Some(completed));
            }
        }
        Ok(None)
    }

    fn finish_event(&mut self, event: SseEvent) -> Result<Option<Vec<u8>>, ProviderError> {
        let SseEvent { value, data } = event;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProviderError::InvalidStream)?;
        if let Some(preview) = &mut self.preview {
            preview.event(&value).inspect_err(|_| {
                self.stage = Stage::OutcomeContract;
            })?;
        }
        match kind {
            "codex.rate_limits" => {
                limits::event(&value, &self.limits);
                Ok(None)
            }
            "response.created" => self.created(&data),
            "response.completed" => self.completed(&value, data),
            "response.output_item.done" => self.item_done(&value, &data),
            "response.failed" | "error" => {
                self.stage = Stage::RemoteFailure;
                Err(remote_stream_error(&value, kind))
            }
            "response.incomplete" => {
                self.stage = Stage::IncompleteResponse;
                Err(ProviderError::InvalidStream)
            }
            _ => Ok(None),
        }
    }

    fn created(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        self.stage = Stage::CompletedResponse;
        let created: CreatedEvent =
            serde_json::from_slice(data).map_err(|_| ProviderError::InvalidResponseContract)?;
        if let Some(response) = created.response {
            if self.response_id.is_some()
                || !self.done_items.is_empty()
                || response.id.is_empty()
                || response.id.len() > 128
                || !response.id.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ProviderError::InvalidResponseContract);
            }
            self.response_id = Some(response.id);
        }
        Ok(None)
    }

    fn completed(
        &mut self,
        value: &Value,
        data: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        self.stage = Stage::CompletedResponse;
        if value.get("response").is_none() {
            return Err(ProviderError::InvalidResponseContract);
        }
        self.output_tokens_details = value
            .pointer("/response/usage/output_tokens_details")
            .cloned();
        if cfg!(debug_assertions)
            && let Some(output) = value.pointer("/response/output").and_then(Value::as_array)
        {
            for item in output {
                if item.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                self.response_shape.messages += 1;
                match item.get("phase").and_then(Value::as_str) {
                    Some("commentary") => self.response_shape.commentary += 1,
                    Some("final_answer") => self.response_shape.finals += 1,
                    None => self.response_shape.unphased += 1,
                    _ => {}
                }
            }
        }
        if !self.done_items.is_empty() {
            let id = self
                .response_id
                .as_deref()
                .ok_or(ProviderError::InvalidResponseContract)?;
            let completed =
                wire::reconcile_streamed_output(&data, id, std::mem::take(&mut self.done_items))
                    .map_err(|error| {
                        self.stage = match error {
                            wire::ResponseError::Identifier => Stage::ResponseIdentifier,
                            wire::ResponseError::Message => Stage::ResponseMessage,
                            _ => Stage::ResponseEnvelope,
                        };
                        error.provider_error()
                    })?;
            self.stage = Stage::StreamLimit;
            if completed.len() > MAX_RESPONSE_BYTES {
                return Err(ProviderError::InvalidStream);
            }
            Ok(Some(completed))
        } else {
            if self
                .response_id
                .as_deref()
                .is_some_and(|id| value.pointer("/response/id").and_then(Value::as_str) != Some(id))
            {
                self.stage = Stage::ResponseIdentifier;
                return Err(ProviderError::InvalidResponseContract);
            }
            Ok(Some(data))
        }
    }

    fn item_done(&mut self, value: &Value, data: &[u8]) -> Result<Option<Vec<u8>>, ProviderError> {
        if cfg!(debug_assertions)
            && value.pointer("/item/type").and_then(Value::as_str) == Some("message")
        {
            self.response_shape.done_messages += 1;
            if value.pointer("/item/phase").and_then(Value::as_str) == Some("final_answer") {
                self.response_shape.done_finals += 1;
            }
        }
        self.stage = Stage::CompletedResponse;
        let done: DoneEvent =
            serde_json::from_slice(data).map_err(|_| ProviderError::InvalidResponseContract)?;
        if self.response_id.is_none()
            || done.output_index != self.done_items.len()
            || done.output_index >= MAX_EVENTS
            || done.item.completed_id().is_none()
            || self
                .done_items
                .iter()
                .any(|(item, _)| item.completed_id() == done.item.completed_id())
        {
            return Err(ProviderError::InvalidResponseContract);
        }
        let raw = value
            .get("item")
            .cloned()
            .ok_or(ProviderError::InvalidResponseContract)?;
        self.done_items.push((done.item, raw));
        Ok(None)
    }

    pub(super) fn set_limits(&mut self, limits: RateLimitSlot) {
        self.limits = limits;
    }

    pub(super) fn stage(&self) -> Stage {
        self.stage
    }

    pub(super) fn event_count(&self) -> usize {
        self.framer.events
    }

    pub(super) fn end_error(&self) -> ProviderError {
        self.framer.end_error()
    }

    pub(super) fn output_tokens_details(&self) -> Option<&Value> {
        self.output_tokens_details.as_ref()
    }

    pub(super) fn counts(&self) -> StreamCounts {
        StreamCounts {
            bytes: self.framer.bytes,
            events: self.framer.events,
            http_status: Some(200),
            response_shape: Some(self.response_shape),
            tool_arguments: None,
        }
    }
}

fn remote_stream_error(value: &Value, event: &str) -> ProviderError {
    let code = match event {
        "response.failed" => value.pointer("/response/error/code"),
        "error" => value.get("code"),
        _ => None,
    }
    .and_then(Value::as_str);
    match code {
        Some(code)
            if !code.is_empty()
                && code.len() <= 128
                && code.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                }) =>
        {
            ProviderError::RemoteStreamCode(code.to_owned())
        }
        _ => ProviderError::Rejected,
    }
}

use super::{MAX_RESPONSE_BYTES, ProviderError};
use crate::session::{MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;

const MAX_OUTPUT_ITEMS: usize = 8192;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewText {
    pub answer: String,
    pub commentary: Vec<String>,
    pub commentary_limited: bool,
    /// The answer text may still become commentary, so it is not journaled before completion.
    pub tentative: bool,
}

#[derive(Clone)]
pub struct ResponsePreview {
    sender: watch::Sender<Arc<PreviewText>>,
}

impl ResponsePreview {
    pub(crate) fn new() -> (Self, watch::Receiver<Arc<PreviewText>>) {
        let (sender, receiver) = watch::channel(Arc::new(PreviewText::default()));
        (Self { sender }, receiver)
    }

    // A same-route retry withdraws the failed attempt's provisional text before it re-dispatches.
    pub(crate) fn clear(&self) {
        self.sender.send_if_modified(|text| {
            let changed = **text != PreviewText::default();
            if changed {
                *text = Arc::new(PreviewText::default());
            }
            changed
        });
    }

    pub(crate) fn has_output(&self) -> bool {
        let text = self.sender.borrow();
        !text.answer.is_empty() || !text.commentary.is_empty() || text.commentary_limited
    }

    pub fn publish(&self, text: PreviewText) -> Result<(), ProviderError> {
        let prior = self.sender.borrow();
        let withdrawn = text.answer.is_empty() && !prior.answer.is_empty();
        if text.answer.len() > MAX_DISPLAY_TEXT_BYTES
            || !(withdrawn || text.answer.starts_with(&prior.answer))
            || text.commentary.len() > MAX_DISPLAY_COMMENTARY_ITEMS
            || text.commentary.iter().map(String::len).sum::<usize>() > MAX_DISPLAY_TEXT_BYTES
            || text.commentary.len() < prior.commentary.len()
            || prior
                .commentary
                .iter()
                .zip(&text.commentary)
                .any(|(left, right)| left != right)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let changed = **prior != text;
        drop(prior);
        if changed {
            self.sender.send_replace(Arc::new(text));
        }
        Ok(())
    }
}

pub(super) fn visible_prefix<'a>(text: &'a str, secret: &str) -> Result<&'a str, ProviderError> {
    if secret.is_empty() {
        return Ok(text);
    }
    if text.contains(secret) {
        return Err(ProviderError::InvalidOutcome);
    }
    let held = (1..secret.len().min(text.len() + 1))
        .rev()
        .find(|&size| text.as_bytes().ends_with(&secret.as_bytes()[..size]))
        .unwrap_or(0);
    let mut end = text.len() - held;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(&text[..end])
}

#[derive(Default)]
pub(super) struct TextDecoder {
    text: String,
}

impl TextDecoder {
    pub(super) fn new() -> Self {
        Self::default()
    }
    pub(super) fn feed(&mut self, fragment: &str, secret: &str) -> Result<&str, ProviderError> {
        if self.text.len().saturating_add(fragment.len()) > MAX_DISPLAY_TEXT_BYTES {
            return Err(ProviderError::InvalidOutcome);
        }
        self.text.push_str(fragment);
        visible_prefix(&self.text, secret)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FramingStage {
    Limit,
    Framing,
    Json,
    Credential,
    Type,
}

pub(super) struct SseEvent {
    pub(super) value: Value,
    pub(super) data: Vec<u8>,
}

pub(super) struct SseFramer {
    pub(super) bytes: usize,
    pub(super) events: usize,
    max_bytes: usize,
    max_events: usize,
    line: Vec<u8>,
    data: Vec<u8>,
    event: Option<String>,
    after_cr: bool,
    pub(super) stage: FramingStage,
}

impl SseFramer {
    // A clean end after a well-formed event is a cut, retried on the same route; a body that
    // never framed an event is not a stream.
    pub(super) fn end_error(&self) -> ProviderError {
        if self.events > 0 {
            ProviderError::StreamEnded
        } else {
            ProviderError::InvalidStream
        }
    }

    pub(super) fn new(max_output_tokens: u32) -> Self {
        Self {
            max_bytes: super::max_stream_bytes(max_output_tokens),
            max_events: super::max_stream_events(max_output_tokens),
            bytes: 0,
            events: 0,
            line: Vec::new(),
            data: Vec::new(),
            event: None,
            after_cr: false,
            stage: FramingStage::Framing,
        }
    }

    pub(super) fn admit(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.stage = FramingStage::Limit;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|size| *size <= self.max_bytes)
            .ok_or_else(|| self.limit_error())?;
        Ok(())
    }

    fn limit_error(&self) -> ProviderError {
        crate::provider::diagnostics::stream_limit(self.bytes, self.events);
        ProviderError::InvalidStream
    }

    pub(super) fn byte(
        &mut self,
        byte: u8,
        secret: &str,
    ) -> Result<Option<SseEvent>, ProviderError> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if matches!(byte, b'\r' | b'\n') {
            self.after_cr = byte == b'\r';
            return self.end_line(secret);
        }
        self.stage = FramingStage::Limit;
        if self.line.len() == MAX_RESPONSE_BYTES {
            return Err(self.limit_error());
        }
        self.line.push(byte);
        Ok(None)
    }

    fn end_line(&mut self, secret: &str) -> Result<Option<SseEvent>, ProviderError> {
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.field(&line)?;
            return Ok(None);
        }
        if self.data.is_empty() && self.event.is_none() {
            return Ok(None);
        }
        self.events += 1;
        if self.events > self.max_events {
            self.stage = FramingStage::Limit;
            return Err(self.limit_error());
        }
        self.stage = FramingStage::Framing;
        if self.data.is_empty() {
            return Err(ProviderError::InvalidStream);
        }
        let data = std::mem::take(&mut self.data);
        let event = self.event.take();
        self.stage = FramingStage::Credential;
        if !secret.is_empty()
            && data
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        {
            return Err(ProviderError::InvalidOutcome);
        }
        self.stage = FramingStage::Json;
        let value = super::parse_response_json(&data).map_err(|_| ProviderError::InvalidStream)?;
        self.stage = FramingStage::Credential;
        if reflects_secret(&value, secret) {
            return Err(ProviderError::InvalidOutcome);
        }
        self.stage = FramingStage::Type;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProviderError::InvalidStream)?;
        if !is_event_type(kind) || event.as_deref().is_some_and(|event| event != kind) {
            return Err(ProviderError::InvalidStream);
        }
        Ok(Some(SseEvent { value, data }))
    }

    fn field(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        self.stage = FramingStage::Framing;
        if line.contains(&0) {
            return Err(ProviderError::InvalidStream);
        }
        if line.starts_with(b":") {
            return Ok(());
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Ok(());
        };
        let (field, value) = (&line[..colon], &line[colon + 1..]);
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"data" => {
                self.stage = FramingStage::Limit;
                let separator = usize::from(!self.data.is_empty());
                if self.data.len() + separator + value.len() > MAX_RESPONSE_BYTES {
                    return Err(self.limit_error());
                }
                if separator > 0 {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
            }
            b"event" if self.event.is_none() => {
                let name = std::str::from_utf8(value).map_err(|_| ProviderError::InvalidStream)?;
                if !is_event_type(name) {
                    return Err(ProviderError::InvalidStream);
                }
                self.event = Some(name.to_owned());
            }
            b"event" => return Err(ProviderError::InvalidStream),
            _ => {}
        }
        Ok(())
    }
}

fn is_event_type(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(super) struct ResponsesPreview<'a> {
    secret: &'a str,
    model: &'a str,
    publisher: ResponsePreview,
    done: std::collections::BTreeMap<usize, Value>,
    started: std::collections::HashMap<String, (usize, String, bool)>,
    live: Option<LiveAnswer>,
}

// Text that may precede a function call streams as the answer candidate and is withdrawn at completion if a call follows.
struct LiveAnswer {
    item_id: String,
    decoder: TextDecoder,
    visible: String,
    failed: bool,
    tentative: bool,
}

impl<'a> ResponsesPreview<'a> {
    pub(super) fn new(secret: &'a str, model: &'a str, publisher: ResponsePreview) -> Self {
        Self {
            secret,
            model,
            publisher,
            done: Default::default(),
            started: Default::default(),
            live: None,
        }
    }

    pub(super) fn event(&mut self, value: &Value) -> Result<(), ProviderError> {
        match value["type"].as_str() {
            Some("response.created")
                if value
                    .pointer("/response/model")
                    .and_then(Value::as_str)
                    .is_some_and(|model| model != self.model) =>
            {
                Err(ProviderError::InvalidResponseContract)
            }
            Some("response.output_item.added") => self.item_added(value),
            Some("response.output_text.delta" | "response.function_call_arguments.delta") => {
                self.delta(value)
            }
            Some("response.output_item.done") => self.item_done(value),
            Some("response.completed") => self.completed(value),
            _ => Ok(()),
        }
    }

    fn item_added(&mut self, value: &Value) -> Result<(), ProviderError> {
        let item = &value["item"];
        let id = item["id"]
            .as_str()
            .filter(|id| {
                !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or(ProviderError::InvalidStream)?;
        let index = value["output_index"]
            .as_u64()
            .filter(|index| *index < MAX_OUTPUT_ITEMS as u64)
            .ok_or(ProviderError::InvalidStream)? as usize;
        let kind = item["type"].as_str().ok_or(ProviderError::InvalidStream)?;
        if self.started.len() >= MAX_OUTPUT_ITEMS
            || self.started.contains_key(id)
            || self.started.values().any(|(prior, _, _)| *prior == index)
            || kind == "message"
                && (!matches!(
                    item["phase"].as_str(),
                    None | Some("commentary" | "final_answer")
                ) || item["role"] != "assistant")
        {
            return Err(ProviderError::InvalidResponseContract);
        }
        self.started
            .insert(id.to_owned(), (index, kind.to_owned(), false));
        if kind == "message" && item["phase"] != "commentary" && self.live.is_none() {
            self.live = Some(LiveAnswer {
                item_id: id.to_owned(),
                decoder: TextDecoder::new(),
                visible: String::new(),
                failed: false,
                tentative: item["phase"] != "final_answer",
            });
        }

        Ok(())
    }

    fn delta(&mut self, value: &Value) -> Result<(), ProviderError> {
        let id = value["item_id"]
            .as_str()
            .ok_or(ProviderError::InvalidStream)?;
        let (index, kind, closed) = self.started.get(id).ok_or(ProviderError::InvalidStream)?;
        let function = value["type"] == "response.function_call_arguments.delta";
        if *closed
            || value["output_index"].as_u64() != Some(*index as u64)
            || (function && kind != "function_call")
            || (!function && (kind != "message" || value["content_index"].as_u64() != Some(0)))
            || !value["delta"].is_string()
        {
            return Err(ProviderError::InvalidStream);
        }
        if !function
            && let Some(live) = self.live.as_mut().filter(|live| live.item_id == id)
            && !live.failed
        {
            let fragment = value["delta"].as_str().unwrap_or_default();
            match live.decoder.feed(fragment, self.secret) {
                Ok(visible) if visible.len() > live.visible.len() => {
                    live.visible = visible.to_owned();
                    let streamed = PreviewText {
                        answer: live.visible.clone(),
                        tentative: live.tentative,
                        ..Default::default()
                    };
                    live.failed = self.publisher.publish(streamed).is_err();
                }
                Ok(_) => {}
                Err(_) => live.failed = true,
            }
        }

        Ok(())
    }

    fn item_done(&mut self, value: &Value) -> Result<(), ProviderError> {
        let index = value["output_index"]
            .as_u64()
            .filter(|index| *index < MAX_OUTPUT_ITEMS as u64)
            .ok_or(ProviderError::InvalidStream)? as usize;
        if let Some(id) = value["item"]["id"].as_str()
            && let Some((started_index, kind, closed)) = self.started.get_mut(id)
        {
            if *closed || *started_index != index || value["item"]["type"] != *kind {
                return Err(ProviderError::InvalidStream);
            }
            *closed = true;
        }
        if self.done.insert(index, value["item"].clone()).is_some() {
            return Err(ProviderError::InvalidStream);
        }

        Ok(())
    }

    // Without a call or a final message the last commentary is the answer.
    fn completed(&mut self, value: &Value) -> Result<(), ProviderError> {
        if self.started.values().any(|(_, _, closed)| !closed) {
            return Err(ProviderError::InvalidStream);
        }
        let output = value
            .pointer("/response/output")
            .and_then(Value::as_array)
            .ok_or(ProviderError::InvalidStream)?;
        let items: Vec<_> = if output.is_empty() {
            self.done.values().collect()
        } else {
            output.iter().collect()
        };
        let has_call = items.iter().any(|item| item["type"] == "function_call");
        let messages = || {
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| item["type"] == "message" && item["role"] == "assistant")
        };
        let commentary_answer = (!has_call
            && messages().all(|(_, item)| item["phase"] == "commentary"))
        .then(|| messages().next_back().map(|(index, _)| index))
        .flatten();
        let mut preview = PreviewText::default();
        for (index, item) in messages() {
            let Some(parts) = item["content"].as_array() else {
                return Err(ProviderError::InvalidResponseContract);
            };
            let mut text = String::new();
            for part in parts {
                if part["type"] != "output_text" {
                    return Err(ProviderError::InvalidResponseContract);
                }
                text.push_str(part["text"].as_str().ok_or(ProviderError::InvalidStream)?);
            }
            if !self.secret.is_empty() && text.contains(self.secret) {
                return Err(ProviderError::InvalidOutcome);
            }
            if has_call || item["phase"] == "commentary" && Some(index) != commentary_answer {
                if preview.commentary.len() < MAX_DISPLAY_COMMENTARY_ITEMS
                    && preview.commentary.iter().map(String::len).sum::<usize>() + text.len()
                        <= MAX_DISPLAY_TEXT_BYTES
                {
                    preview.commentary.push(text);
                } else {
                    preview.commentary_limited = true;
                }
            } else if text.len() <= MAX_DISPLAY_TEXT_BYTES {
                preview.answer = text;
            } else if let Some(live) = &self.live {
                preview.answer.clone_from(&live.visible);
            }
        }
        self.publisher.publish(preview)?;

        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct StreamResponseError {
    pub(super) error: ProviderError,
    pub(super) stage: crate::provider::diagnostics::SubscriptionFailureStage,
}

pub(super) fn validate_stream_response(
    response: &reqwest::Response,
    max_output_tokens: u32,
) -> Result<(), StreamResponseError> {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    use reqwest::header;
    let stage = if response
        .content_length()
        .is_some_and(|length| length > super::max_stream_bytes(max_output_tokens) as u64)
    {
        crate::provider::diagnostics::stream_limit(0, 0);
        Stage::ContentLength
    } else if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        Stage::ContentEncoding
    } else if response
        .headers()
        .get_all(header::CONTENT_TYPE)
        .iter()
        .count()
        != 1
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
    {
        Stage::ResponseContent
    } else {
        return Ok(());
    };
    Err(StreamResponseError {
        error: ProviderError::InvalidStream,
        stage,
    })
}

pub(super) fn reflects_secret(value: &Value, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(values) => values.iter().any(|value| reflects_secret(value, secret)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(secret) || reflects_secret(value, secret)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn larger_streams_keep_single_line_and_multiline_event_bounds() {
        let cap = crate::provider::MAX_OUTPUT_TOKEN_CAP;
        let mut framer = SseFramer::new(cap);
        let oversized_line = vec![b' '; MAX_RESPONSE_BYTES + 1];
        framer.admit(oversized_line.len()).unwrap();
        assert!(
            oversized_line
                .into_iter()
                .try_for_each(|byte| { framer.byte(byte, "").map(|_| ()) })
                .is_err()
        );
        assert_eq!(framer.stage, FramingStage::Limit);
        let mut framer = SseFramer::new(cap);
        let line = format!("data: {}\n", " ".repeat(MAX_RESPONSE_BYTES / 2));
        framer.admit(line.len()).unwrap();
        for byte in line.bytes() {
            assert!(framer.byte(byte, "").unwrap().is_none());
        }
        framer.admit(line.len()).unwrap();
        assert!(
            line.bytes()
                .try_for_each(|byte| { framer.byte(byte, "").map(|_| ()) })
                .is_err()
        );
        assert_eq!(framer.stage, FramingStage::Limit);
    }

    #[test]
    fn text_fragments_hold_selected_credential_prefixes() {
        let mut decoder = TextDecoder::new();
        assert_eq!(
            decoder.feed("Hola 🚀 safe syn", "synthetic-key").unwrap(),
            "Hola 🚀 safe "
        );
        assert_eq!(
            decoder.feed("thetic-", "synthetic-key").unwrap(),
            "Hola 🚀 safe "
        );
        assert!(decoder.feed("key", "synthetic-key").is_err());
        assert!(
            TextDecoder::new()
                .feed(&"x".repeat(MAX_DISPLAY_TEXT_BYTES + 1), "key")
                .is_err()
        );
    }

    #[test]
    fn framing_rejects_ambiguous_json_and_escaped_credentials_before_observation() {
        for data in [
            r#"{"type":"response.created","type":"response.completed"}"#,
            r#"{"type":"response.created","text":"\u0073ynthetic-key"}"#,
        ] {
            let wire = format!("data: {data}\n\n");
            let mut framer = SseFramer::new(crate::provider::DEFAULT_OUTPUT_TOKEN_CAP);
            framer.admit(wire.len()).unwrap();
            assert!(
                wire.bytes()
                    .try_for_each(|byte| framer.byte(byte, "synthetic-key").map(|_| ()))
                    .is_err()
            );
        }
        let mut framer = SseFramer::new(crate::provider::DEFAULT_OUTPUT_TOKEN_CAP);
        let mut events = Vec::new();
        let wire = b": heartbeat\r\ndata: {\r\ndata: \"type\":\"ping\"}\r\n\r\n";
        framer.admit(wire.len()).unwrap();
        for &byte in wire {
            if let Some(event) = framer.byte(byte, "key").unwrap() {
                events.push(event.value);
            }
        }
        assert_eq!(events, vec![serde_json::json!({"type":"ping"})]);
    }
}

use super::{MAX_RESPONSE_BYTES, ProviderError};
use crate::session::{MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewText {
    pub answer: String,
    pub commentary: Vec<String>,
    pub commentary_limited: bool,
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

    pub fn publish(&self, text: PreviewText) -> Result<(), ProviderError> {
        let prior = self.sender.borrow();
        if text.answer.len() > MAX_DISPLAY_TEXT_BYTES
            || !text.answer.starts_with(&prior.answer)
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

#[derive(Clone, Copy)]
enum Scope {
    Root,
    Outcome,
    Other,
}

struct Object {
    scope: Scope,
    key: Option<String>,
    expecting_key: bool,
}

struct StringToken {
    raw: Vec<u8>,
    escaped: bool,
    key: bool,
    result: bool,
    decoded: usize,
}

pub(super) struct ResultDecoder {
    objects: Vec<Object>,
    string: Option<StringToken>,
    function: bool,
    result_started: bool,
    result: String,
    total: usize,
}

impl ResultDecoder {
    pub(super) fn new(function: bool) -> Self {
        Self {
            objects: Vec::new(),
            string: None,
            function,
            result_started: false,
            result: String::new(),
            total: 0,
        }
    }

    pub(super) fn feed(&mut self, fragment: &str, secret: &str) -> Result<&str, ProviderError> {
        self.total = self
            .total
            .checked_add(fragment.len())
            .filter(|total| *total <= MAX_RESPONSE_BYTES)
            .ok_or(ProviderError::InvalidStream)?;
        for byte in fragment.bytes() {
            if let Some(token) = &mut self.string {
                if byte == b'"' && !token.escaped {
                    let mut quoted = Vec::with_capacity(token.raw.len() + 2);
                    quoted.push(b'"');
                    quoted.extend_from_slice(&token.raw);
                    quoted.push(b'"');
                    let value: String = serde_json::from_slice(&quoted)
                        .map_err(|_| ProviderError::InvalidOutcome)?;
                    if !secret.is_empty() && value.contains(secret) {
                        return Err(ProviderError::InvalidOutcome);
                    }
                    if token.key {
                        let object = self
                            .objects
                            .last_mut()
                            .ok_or(ProviderError::InvalidOutcome)?;
                        object.key = Some(value);
                        object.expecting_key = false;
                    }
                    self.string = None;
                    continue;
                }
                token.raw.push(byte);
                if token.result {
                    decode_string_prefix(&token.raw, &mut token.decoded, &mut self.result)?;
                    if self.result.len() > MAX_DISPLAY_TEXT_BYTES {
                        return Err(ProviderError::InvalidOutcome);
                    }
                }
                token.escaped = byte == b'\\' && !token.escaped;
                continue;
            }
            match byte {
                b'{' | b'[' => {
                    if self.objects.len() >= 64 {
                        return Err(ProviderError::InvalidStream);
                    }
                    let scope = if self.objects.is_empty() && byte == b'{' {
                        Scope::Root
                    } else if byte == b'{'
                        && self.objects.last().is_some_and(|object| {
                            matches!(object.scope, Scope::Root)
                                && object.key.as_deref() == Some("outcome")
                        })
                    {
                        Scope::Outcome
                    } else {
                        Scope::Other
                    };
                    self.objects.push(Object {
                        scope,
                        key: None,
                        expecting_key: byte == b'{',
                    });
                }
                b'}' | b']' => {
                    self.objects.pop().ok_or(ProviderError::InvalidOutcome)?;
                }
                b',' => {
                    if let Some(object) = self.objects.last_mut() {
                        object.key = None;
                        object.expecting_key = !matches!(object.scope, Scope::Other);
                    }
                }
                b'"' => {
                    let object = self.objects.last().ok_or(ProviderError::InvalidOutcome)?;
                    let result = !object.expecting_key
                        && object.key.as_deref() == Some("result")
                        && (matches!(object.scope, Scope::Outcome)
                            || self.function && matches!(object.scope, Scope::Root));
                    if result && self.result_started {
                        return Err(ProviderError::InvalidOutcome);
                    }
                    self.result_started |= result;
                    self.string = Some(StringToken {
                        raw: Vec::new(),
                        escaped: false,
                        key: object.expecting_key,
                        result,
                        decoded: 0,
                    });
                }
                _ => {}
            }
        }
        visible_prefix(&self.result, secret)
    }
}

fn decode_string_prefix(
    raw: &[u8],
    offset: &mut usize,
    text: &mut String,
) -> Result<(), ProviderError> {
    while *offset < raw.len() {
        let remaining = &raw[*offset..];
        let length = match remaining[0] {
            b'\\' => {
                if remaining.len() < 2 {
                    break;
                }
                if remaining[1] == b'u' {
                    if remaining.len() < 6 {
                        break;
                    }
                    let digits = std::str::from_utf8(&remaining[2..6])
                        .map_err(|_| ProviderError::InvalidOutcome)?;
                    let value = u16::from_str_radix(digits, 16)
                        .map_err(|_| ProviderError::InvalidOutcome)?;
                    if (0xd800..=0xdbff).contains(&value) {
                        12
                    } else {
                        6
                    }
                } else {
                    2
                }
            }
            0..=31 | b'"' => return Err(ProviderError::InvalidOutcome),
            32..=127 => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(ProviderError::InvalidOutcome),
        };
        if remaining.len() < length {
            break;
        }
        if remaining[0] == b'\\' {
            let mut quoted = Vec::with_capacity(length + 2);
            quoted.push(b'"');
            quoted.extend_from_slice(&remaining[..length]);
            quoted.push(b'"');
            let decoded: String =
                serde_json::from_slice(&quoted).map_err(|_| ProviderError::InvalidOutcome)?;
            text.push_str(&decoded);
        } else {
            text.push_str(
                std::str::from_utf8(&remaining[..length])
                    .map_err(|_| ProviderError::InvalidOutcome)?,
            );
        }
        *offset += length;
    }
    Ok(())
}

#[derive(Clone, Copy)]
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
    line: Vec<u8>,
    data: Vec<u8>,
    event: Option<String>,
    after_cr: bool,
    pub(super) stage: FramingStage,
}

impl SseFramer {
    pub(super) fn new() -> Self {
        Self {
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
            .filter(|size| *size <= MAX_RESPONSE_BYTES)
            .ok_or(ProviderError::InvalidStream)?;
        Ok(())
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
            let line = std::mem::take(&mut self.line);
            if !line.is_empty() {
                self.stage = FramingStage::Framing;
                if line.contains(&0) {
                    return Err(ProviderError::InvalidStream);
                }
                if line.starts_with(b":") {
                    return Ok(None);
                }
                let Some(colon) = line.iter().position(|byte| *byte == b':') else {
                    return Ok(None);
                };
                let (field, value) = (&line[..colon], &line[colon + 1..]);
                let value = value.strip_prefix(b" ").unwrap_or(value);
                match field {
                    b"data" => {
                        if !self.data.is_empty() {
                            self.data.push(b'\n');
                        }
                        self.data.extend_from_slice(value);
                    }
                    b"event" if self.event.is_none() => {
                        let name =
                            std::str::from_utf8(value).map_err(|_| ProviderError::InvalidStream)?;
                        if name.is_empty()
                            || name.len() > 128
                            || !name.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                            })
                        {
                            return Err(ProviderError::InvalidStream);
                        }
                        self.event = Some(name.to_owned());
                    }
                    b"event" => return Err(ProviderError::InvalidStream),
                    _ => {}
                }
                return Ok(None);
            }
            if self.data.is_empty() && self.event.is_none() {
                return Ok(None);
            }
            self.events += 1;
            self.stage = FramingStage::Limit;
            if self.events > 8192 || self.data.is_empty() {
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
            let value = crate::tools::parse_json(&data, MAX_RESPONSE_BYTES)
                .map_err(|_| ProviderError::InvalidStream)?;
            self.stage = FramingStage::Credential;
            if reflects_secret(&value, secret) {
                return Err(ProviderError::InvalidOutcome);
            }
            self.stage = FramingStage::Type;
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or(ProviderError::InvalidStream)?;
            if kind.is_empty()
                || kind.len() > 128
                || !kind
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || event.as_deref().is_some_and(|event| event != kind)
            {
                return Err(ProviderError::InvalidStream);
            }
            return Ok(Some(SseEvent { value, data }));
        }
        self.stage = FramingStage::Limit;
        if self.line.len() == MAX_RESPONSE_BYTES {
            return Err(ProviderError::InvalidStream);
        }
        self.line.push(byte);
        Ok(None)
    }
}

enum ResponseItem {
    Result(ResultDecoder, bool),
    Commentary,
    Other,
}

pub(super) struct ResponsesPreview<'a> {
    secret: &'a str,
    model: &'a str,
    publisher: ResponsePreview,
    items: std::collections::HashMap<String, (usize, ResponseItem, bool)>,
    text: PreviewText,
    result_item: Option<String>,
}

impl<'a> ResponsesPreview<'a> {
    pub(super) fn new(secret: &'a str, model: &'a str, publisher: ResponsePreview) -> Self {
        Self {
            secret,
            model,
            publisher,
            items: std::collections::HashMap::new(),
            text: PreviewText::default(),
            result_item: None,
        }
    }

    pub(super) fn event(&mut self, value: &Value) -> Result<(), ProviderError> {
        match value.get("type").and_then(Value::as_str) {
            Some("response.created")
                if value
                    .pointer("/response/model")
                    .and_then(Value::as_str)
                    .is_some_and(|model| model != self.model) =>
            {
                return Err(ProviderError::InvalidResponseContract);
            }
            Some("response.output_item.added") => {
                let item = value.get("item").ok_or(ProviderError::InvalidStream)?;
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| {
                        !id.is_empty()
                            && id.len() <= 128
                            && id.bytes().all(|byte| byte.is_ascii_graphic())
                    })
                    .ok_or(ProviderError::InvalidStream)?;
                if self.items.len() >= 8192 || self.items.contains_key(id) {
                    return Err(ProviderError::InvalidStream);
                }
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .filter(|index| *index < 8192)
                    .ok_or(ProviderError::InvalidStream)? as usize;
                if self.items.values().any(|(prior, _, _)| *prior == index) {
                    return Err(ProviderError::InvalidStream);
                }
                let capture = match item.get("type").and_then(Value::as_str) {
                    Some("function_call")
                        if item.get("name").and_then(Value::as_str) == Some("arany_finish") =>
                    {
                        ResponseItem::Result(ResultDecoder::new(true), true)
                    }
                    Some("message")
                        if item.get("role").and_then(Value::as_str) == Some("assistant") =>
                    {
                        match item.get("phase").and_then(Value::as_str) {
                            Some("commentary") => ResponseItem::Commentary,
                            Some("final_answer") | None => {
                                ResponseItem::Result(ResultDecoder::new(false), false)
                            }
                            _ => return Err(ProviderError::InvalidResponseContract),
                        }
                    }
                    _ => ResponseItem::Other,
                };
                if matches!(capture, ResponseItem::Result(_, _)) {
                    if self.result_item.is_some() {
                        return Err(ProviderError::InvalidResponseContract);
                    }
                    self.result_item = Some(id.to_owned());
                }
                self.items.insert(id.to_owned(), (index, capture, false));
            }
            Some("response.output_text.delta" | "response.function_call_arguments.delta") => {
                let id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::InvalidStream)?;
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::InvalidStream)?;
                let (index, item, closed) =
                    self.items.get_mut(id).ok_or(ProviderError::InvalidStream)?;
                if *closed
                    || value.get("output_index").and_then(Value::as_u64) != Some(*index as u64)
                {
                    return Err(ProviderError::InvalidStream);
                }
                if let ResponseItem::Result(decoder, function) = item {
                    if *function != (value["type"] == "response.function_call_arguments.delta")
                        || !*function
                            && value.get("content_index").and_then(Value::as_u64) != Some(0)
                    {
                        return Err(ProviderError::InvalidStream);
                    }
                    self.text.answer = decoder.feed(delta, self.secret)?.to_owned();
                    self.publisher.publish(self.text.clone())?;
                }
            }
            Some("response.output_item.done") => {
                let item = value.get("item").ok_or(ProviderError::InvalidStream)?;
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::InvalidStream)?;
                let (index, captured, closed) =
                    self.items.get_mut(id).ok_or(ProviderError::InvalidStream)?;
                if *closed
                    || value.get("output_index").and_then(Value::as_u64) != Some(*index as u64)
                    || matches!(captured, ResponseItem::Commentary)
                        != (item["phase"] == "commentary")
                {
                    return Err(ProviderError::InvalidStream);
                }
                *closed = true;
                if item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some("assistant")
                    && item.get("status").and_then(Value::as_str) == Some("completed")
                    && item.get("phase").and_then(Value::as_str) == Some("commentary")
                {
                    let parts = item
                        .get("content")
                        .and_then(Value::as_array)
                        .ok_or(ProviderError::InvalidStream)?;
                    let mut text = String::new();
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) != Some("output_text") {
                            return Err(ProviderError::InvalidResponseContract);
                        }
                        text.push_str(
                            part.get("text")
                                .and_then(Value::as_str)
                                .ok_or(ProviderError::InvalidStream)?,
                        );
                    }
                    if !self.secret.is_empty() && text.contains(self.secret) {
                        return Err(ProviderError::InvalidOutcome);
                    }
                    if self.text.commentary.len() == MAX_DISPLAY_COMMENTARY_ITEMS
                        || self.text.commentary.iter().map(String::len).sum::<usize>() + text.len()
                            > MAX_DISPLAY_TEXT_BYTES
                    {
                        self.text.commentary_limited = true;
                    } else if !text.is_empty() {
                        self.text.commentary.push(text);
                    }
                    self.publisher.publish(self.text.clone())?;
                }
            }
            Some("response.completed") if self.items.values().any(|(_, _, closed)| !closed) => {
                return Err(ProviderError::InvalidStream);
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) fn validate_stream_response(response: &reqwest::Response) -> Result<(), ProviderError> {
    use reqwest::header;
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        || response
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
        return Err(ProviderError::InvalidStream);
    }
    Ok(())
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
    fn result_fragments_decode_unicode_and_hold_selected_credential_prefixes() {
        let cases = [
            (
                false,
                r#"{"outcome":{"result":"a\n\uD83D\uDE80é","summary":"ok","type":"finish"}}"#,
                "a\n🚀é",
            ),
            (true, r#"{"summary":"ok","result":"a\"b\\c"}"#, "a\"b\\c"),
            (
                false,
                r#"{"other":{"result":"hidden"},"outcome":{"type":"finish","result":"safe"}}"#,
                "safe",
            ),
        ];
        for (function, wire, expected) in cases {
            let mut decoder = ResultDecoder::new(function);
            let mut observed = String::new();
            for character in wire.chars() {
                let next = decoder
                    .feed(&character.to_string(), "synthetic-key")
                    .unwrap();
                assert!(next.starts_with(&observed));
                observed = next.to_owned();
            }
            assert_eq!(observed, expected);
        }
        let mut decoder = ResultDecoder::new(false);
        assert_eq!(
            decoder
                .feed(r#"{"outcome":{"result":"safe syn"#, "synthetic-key")
                .unwrap(),
            "safe "
        );
        assert_eq!(decoder.feed("thetic-", "synthetic-key").unwrap(), "safe ");
        assert!(decoder.feed("key", "synthetic-key").is_err());
        for wire in [
            r#"{"outcome":{"result":"\uDC00"}}"#,
            r#"{"outcome":{"result":"a","result":"b"}}"#,
            r#"{"outcome":{"result":"\x"}}"#,
        ] {
            assert!(
                ResultDecoder::new(false).feed(wire, "key").is_err(),
                "{wire}"
            );
        }
    }

    #[test]
    fn framing_rejects_ambiguous_json_and_escaped_credentials_before_observation() {
        for data in [
            r#"{"type":"response.created","type":"response.completed"}"#,
            r#"{"type":"response.created","text":"\u0073ynthetic-key"}"#,
        ] {
            let wire = format!("data: {data}\n\n");
            let mut framer = SseFramer::new();
            framer.admit(wire.len()).unwrap();
            assert!(
                wire.bytes()
                    .try_for_each(|byte| framer.byte(byte, "synthetic-key").map(|_| ()))
                    .is_err()
            );
        }
        let mut framer = SseFramer::new();
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

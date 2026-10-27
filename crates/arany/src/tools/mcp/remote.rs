use super::super::ToolError;
use super::super::config::{McpAuth, Server};
use super::super::skills::parse_json;
use super::super::types::{MCP_TIMEOUT_MS, ToolCall};
use super::oauth::MAX_ACCESS_TOKEN_BYTES;
use super::{FRAME_BYTES, Session, VERSION, Wire, converse, encode};
use reqwest::header::{ACCEPT, CONTENT_ENCODING, CONTENT_TYPE};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;

// One HTTP response carries a few frames at most: the reply plus bounded server notifications.
pub(crate) const MAX_MCP_RESPONSE_BYTES: usize = 4 * FRAME_BYTES;
const MAX_RESPONSE_MESSAGES: usize = 16;
const MAX_SESSION_ID_BYTES: usize = 256;

struct Stream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    bytes: usize,
    frames: usize,
}

struct Http {
    client: reqwest::Client,
    url: reqwest::Url,
    bearer: Option<String>,
    session_id: Option<String>,
    inbox: VecDeque<Value>,
    stream: Option<Stream>,
    unauthorized: bool,
    // `tools/call` may have reached the server; it is settled only once its reply arrived intact.
    dispatched: bool,
    call_id: Value,
    settled: bool,
}

impl Http {
    // Only a well-formed reply to the dispatched call proves the server answered it.
    fn answers_call(&self, frame: &Value) -> bool {
        self.dispatched
            && frame["jsonrpc"] == "2.0"
            && frame.get("method").is_none()
            && frame["id"] == self.call_id
            && frame.get("result").is_some() != frame.get("error").is_some()
    }

    fn accept(&mut self, frame: Value) -> Result<(), ToolError> {
        if self
            .bearer
            .as_deref()
            .is_some_and(|token| mentions(&frame, token))
        {
            return Err(ToolError::Operation);
        }
        if self.answers_call(&frame) {
            self.settled = true;
        }
        self.inbox.push_back(frame);
        Ok(())
    }
}

impl Wire for Http {
    async fn send(&mut self, message: Value) -> Result<(), ToolError> {
        if message.get("method").is_some() && message.get("id").is_some() {
            self.stream = None;
            self.inbox.clear();
        }
        let is_call = message["method"] == "tools/call";
        if is_call {
            self.dispatched = true;
            self.call_id = message["id"].clone();
        }
        let mut request = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", VERSION)
            .body(encode(&message)?);
        if let Some(token) = &self.bearer {
            request = request.bearer_auth(token);
        }
        if let Some(id) = &self.session_id {
            request = request.header("Mcp-Session-Id", id);
        }
        let mut response = request.send().await.map_err(|_| ToolError::Operation)?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            // A refusal of an auxiliary POST (a ping reply) says nothing about whether the call already ran.
            self.unauthorized = is_call || !self.dispatched;
            return Err(ToolError::Operation);
        }
        if self.session_id.is_none()
            && let Some(id) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
        {
            if id.is_empty()
                || id.len() > MAX_SESSION_ID_BYTES
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(ToolError::Operation);
            }
            self.session_id = Some(id.to_owned());
        }
        if status.as_u16() == 202 {
            return Ok(());
        }
        if !status.is_success()
            || response
                .headers()
                .get_all(CONTENT_ENCODING)
                .iter()
                .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(ToolError::Operation);
        }
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        match kind.as_str() {
            "application/json" => {
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| ToolError::Operation)? {
                    if body.len() + chunk.len() > MAX_MCP_RESPONSE_BYTES {
                        return Err(ToolError::Limit);
                    }
                    body.extend_from_slice(&chunk);
                }
                self.accept(parse_json(&body, FRAME_BYTES)?)
            }
            // Frames are serviced as they arrive: a server may wait for a `ping` reply before it answers.
            "text/event-stream" => {
                self.stream = Some(Stream {
                    response,
                    buffer: Vec::new(),
                    bytes: 0,
                    frames: 0,
                });
                Ok(())
            }
            _ => Err(ToolError::Operation),
        }
    }

    async fn receive(&mut self) -> Result<Value, ToolError> {
        loop {
            if let Some(frame) = self.inbox.pop_front() {
                return Ok(frame);
            }
            let stream = self.stream.as_mut().ok_or(ToolError::Operation)?;
            let chunk = stream
                .response
                .chunk()
                .await
                .map_err(|_| ToolError::Operation)?;
            let frames = match chunk {
                Some(chunk) => {
                    stream.bytes += chunk.len();
                    if stream.bytes > MAX_MCP_RESPONSE_BYTES {
                        return Err(ToolError::Limit);
                    }
                    stream.buffer.extend_from_slice(&chunk);
                    drain_events(&mut stream.buffer)
                }
                None => {
                    let rest = std::mem::take(&mut stream.buffer);
                    self.stream = None;
                    event_data(&rest)
                }
            };
            let streamed = self.stream.as_mut().map(|stream| {
                stream.frames += frames.len();
                stream.frames
            });
            if streamed.unwrap_or(frames.len()) > MAX_RESPONSE_MESSAGES {
                return Err(ToolError::Limit);
            }
            for frame in frames {
                self.accept(parse_json(&frame, FRAME_BYTES)?)?;
            }
        }
    }
}

// The token is checked on decoded text, so an escaped spelling of it is caught too.
fn mentions(value: &Value, token: &str) -> bool {
    match value {
        Value::String(text) => text.contains(token),
        Value::Array(items) => items.iter().any(|item| mentions(item, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, item)| key.contains(token) || mentions(item, token)),
        _ => false,
    }
}

// Splits off the complete events; a trailing partial event waits for the next chunk.
fn drain_events(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut normalized = Vec::with_capacity(buffer.len());
    for (index, byte) in buffer.iter().enumerate() {
        if !(*byte == b'\r' && buffer.get(index + 1) == Some(&b'\n')) {
            normalized.push(*byte);
        }
    }
    *buffer = normalized;
    match buffer.windows(2).rposition(|pair| pair == b"\n\n") {
        Some(end) => {
            let complete: Vec<u8> = buffer.drain(..end + 2).collect();
            event_data(&complete)
        }
        None => Vec::new(),
    }
}

// Joins each event's `data:` lines; comments, ids, event names and retry hints carry no message.
fn event_data(body: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    text.split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
                .collect();
            (!data.is_empty()).then(|| data.join("\n").into_bytes())
        })
        .collect()
}

fn unauthorized(server: &Server) -> String {
    json!({
        "server": server.name,
        "error": "unauthorized",
        "action": format!("run `arany mcp login {}` to authorize this server", server.name),
    })
    .to_string()
}

pub(crate) async fn execute(
    call: &ToolCall,
    server: &Server,
    bearer: Option<&str>,
    result_bytes: usize,
    captured: &std::sync::Mutex<Option<Vec<u8>>>,
) -> Result<String, ToolError> {
    let url = server
        .url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .ok_or(ToolError::Configuration)?;
    if bearer.is_some_and(|token| token.len() > MAX_ACCESS_TOKEN_BYTES) {
        return Err(ToolError::Configuration);
    }
    if server.auth == Some(McpAuth::Oauth) && bearer.is_none() {
        return Ok(unauthorized(server));
    }
    let timeout = Duration::from_millis(u64::from(MCP_TIMEOUT_MS));
    let mut session = Session {
        wire: Http {
            client: super::super::web::client(url.scheme() == "https", timeout)?,
            url,
            bearer: bearer.map(str::to_owned),
            session_id: None,
            inbox: VecDeque::new(),
            stream: None,
            unauthorized: false,
            dispatched: false,
            call_id: Value::Null,
            settled: false,
        },
        next_id: 1,
        notifications: 0,
    };
    match converse(&mut session, call, server, result_bytes, captured).await {
        Err(ToolError::Operation) if session.wire.unauthorized => Ok(unauthorized(server)),
        Err(_) if session.wire.dispatched && !session.wire.settled => Err(ToolError::Uncertain),
        result => result,
    }
}

#[cfg(test)]
mod tests;

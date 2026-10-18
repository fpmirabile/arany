use arany::SessionId;
use arany_cli::protocol::{methods::ClientInfo, rpc::*};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Input {
    Request(AcpRequest),
    Notification(AcpNotification),
    Response(AcpResponse),
    Error(AcpError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpRequest {
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Version,
    pub id: RequestId,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpNotification {
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Version,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpResponse {
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Version,
    #[serde(rename = "id")]
    pub _id: RequestId,
    #[serde(rename = "result")]
    pub _result: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpError {
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Version,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    #[serde(rename = "id")]
    pub _id: Option<RequestId>,
    #[serde(rename = "error")]
    pub _error: AcpErrorObject,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpErrorObject {
    #[serde(rename = "code")]
    pub _code: i64,
    #[serde(rename = "message")]
    pub _message: String,
    #[serde(rename = "data")]
    pub _data: Option<Value>,
}

pub(super) fn decode(line: &[u8]) -> Result<Input, CodecError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::TooLarge);
    }
    serde_json::from_slice(line).map_err(|_| CodecError::Parse)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Initialize {
    #[serde(rename = "protocolVersion")]
    pub _protocol_version: u16,
    pub client_info: Option<Info>,
}

#[derive(Deserialize)]
pub(super) struct Info {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

impl Initialize {
    pub fn client(self) -> Result<ClientInfo, ()> {
        let info = self.client_info.unwrap_or(Info {
            name: "acp-client".into(),
            title: None,
            version: "unknown".into(),
        });
        if [&info.name, &info.version]
            .into_iter()
            .chain(info.title.iter())
            .any(|s| s.len() > 256)
        {
            return Err(());
        }
        Ok(ClientInfo {
            name: info.name,
            title: info.title,
            version: info.version,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Authenticate {
    pub method_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NewSession {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub additional_directories: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Prompt {
    pub session_id: SessionId,
    #[serde(deserialize_with = "blocks")]
    pub prompt: Vec<PromptBlock>,
}

fn blocks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<PromptBlock>, D::Error> {
    crate::cli::bounded_list::<D, PromptBlock, 64>(d, "at most 64 content blocks")
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PromptBlock {
    Text { text: String },
    ResourceLink { name: String, uri: String },
}

impl Prompt {
    pub fn text(&self) -> Result<String, ()> {
        let mut combined = String::new();
        for block in &self.prompt {
            let parts: &[&str] = match block {
                PromptBlock::Text { text } => &[text],
                PromptBlock::ResourceLink { name, uri } => &["[@", name, "](", uri, ")"],
            };
            for part in parts {
                if part.len() > 64 * 1024 - combined.len() {
                    return Err(());
                }
                combined.push_str(part);
            }
        }
        if combined.trim().is_empty() {
            return Err(());
        }
        Ok(combined)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Cancel {
    pub session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CancelRequest {
    pub request_id: RequestId,
}

pub(super) fn initialized() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
            "mcpCapabilities": {"http": false, "sse": false},
            "sessionCapabilities": {}
        },
        "agentInfo": {"name": "arany", "title": "Arany", "version": env!("CARGO_PKG_VERSION")},
        "authMethods": [{"id": "arany-setup", "type": "agent", "name": "Set up Arany",
            "description": "Run `arany --setup` in a terminal"}]
    })
}

pub(super) fn response(id: RequestId, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(super) fn error(id: Option<RequestId>, code: i64, message: &str, reason: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {
        "code": code, "message": message, "data": {"reason": reason}
    }})
}

pub(super) fn auth_required(id: RequestId) -> Value {
    error(
        Some(id),
        -32000,
        "Run `arany --setup` in a terminal",
        "auth_required",
    )
}

pub(super) fn answer(session: SessionId, text: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text}}
    }})
}

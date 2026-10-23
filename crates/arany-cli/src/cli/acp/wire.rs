use arany::{MAX_ENCODED_IMAGE_BYTES, MAX_MESSAGE_IMAGES, MAX_USER_MESSAGE_BYTES, SessionId};
use arany_cli::protocol::{
    methods::{ClientInfo, PermissionMode, deserialize_permission_mode},
    rpc::*,
};
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
    pub id: RequestId,
    pub result: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcpError {
    #[serde(rename = "jsonrpc")]
    pub _jsonrpc: Version,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub id: Option<RequestId>,
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
pub(super) struct LoadSession {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub session: NewSession,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResumeSession {
    pub session_id: SessionId,
    pub cwd: String,
    #[serde(default)]
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
    crate::files::bounded_list::<D, PromptBlock, 64>(d, "at most 64 content blocks")
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PromptBlock {
    Text {
        text: String,
    },
    ResourceLink {
        name: String,
        uri: String,
    },
    Resource {
        resource: TextResource,
    },
    Image {
        #[serde(rename = "mimeType")]
        mime_type: String,
        data: String,
    },
}

#[derive(Deserialize)]
pub(super) struct TextResource {
    pub uri: String,
    pub text: String,
    pub blob: Option<serde::de::IgnoredAny>,
}

impl Prompt {
    pub fn input(self) -> Result<Vec<arany_cli::protocol::methods::UserInput>, ()> {
        use arany_cli::protocol::methods::{ImageMediaType, TurnStartParams, UserInput};
        let mut combined = String::new();
        let mut images = Vec::new();
        for block in &self.prompt {
            let context;
            let parts: &[&str] = match block {
                PromptBlock::Text { text } => &[text],
                PromptBlock::ResourceLink { name, uri } => &["[@", name, "](", uri, ")"],
                PromptBlock::Resource { resource } => {
                    if resource.blob.is_some()
                        || resource.uri.len() > MAX_USER_MESSAGE_BYTES
                        || resource.text.len() > MAX_USER_MESSAGE_BYTES
                    {
                        return Err(());
                    }
                    context = [
                        resource
                            .uri
                            .replace('&', "&amp;")
                            .replace('<', "&lt;")
                            .replace('>', "&gt;")
                            .replace('"', "&quot;"),
                        context_body(&resource.text),
                    ];
                    &[
                        "<context ref=\"",
                        &context[0],
                        "\">",
                        &context[1],
                        "</context>",
                    ]
                }
                PromptBlock::Image { mime_type, data } => {
                    if mime_type != "image/png"
                        || images.len() >= MAX_MESSAGE_IMAGES
                        || data.len() > MAX_ENCODED_IMAGE_BYTES
                    {
                        return Err(());
                    }
                    images.push(UserInput::Image {
                        media_type: ImageMediaType::Png,
                        data: data.clone(),
                    });
                    continue;
                }
            };
            for part in parts {
                if part.len() > MAX_USER_MESSAGE_BYTES - combined.len() {
                    return Err(());
                }
                combined.push_str(part);
            }
        }
        if combined.trim().is_empty() && images.is_empty() {
            return Err(());
        }
        if !combined.is_empty() {
            images.insert(0, UserInput::Text { text: combined });
        }
        // Reuse serve's byte, count and base64 admission before translating the request.
        let params: TurnStartParams = serde_json::from_value(json!({
            "threadId": self.session_id, "input": images
        }))
        .map_err(|_| ())?;
        Ok(params.input)
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
            "loadSession": true,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": false, "sse": false},
            "sessionCapabilities": {"resume": {}, "close": {}, "list": {}}
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

fn context_body(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    let mut start = 0;
    for (index, _) in text.match_indices('<') {
        if text.as_bytes()[index..]
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"</context"))
        {
            safe.push_str(&text[start..index]);
            safe.push_str("&lt;");
            start = index + 1;
        }
    }
    safe.push_str(&text[start..]);
    safe
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SetMode {
    pub session_id: SessionId,
    #[serde(deserialize_with = "deserialize_permission_mode")]
    pub mode_id: PermissionMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SetConfigOption {
    pub session_id: SessionId,
    pub config_id: String,
    #[serde(deserialize_with = "deserialize_permission_mode")]
    pub value: PermissionMode,
    #[serde(rename = "type")]
    pub value_type: Option<String>,
}

pub(super) fn mode_state(mode: &PermissionMode) -> Value {
    let options = [
        (
            PermissionMode::Default,
            "Default",
            "Request one-use approvals",
        ),
        (
            PermissionMode::AcceptEdits,
            "Accept edits",
            "Pre-approve typed edits; commands and MCP still ask",
        ),
        (
            PermissionMode::Auto,
            "Auto",
            "Review approval-needing actions with AI",
        ),
    ];
    json!({
        "modes": {"currentModeId":mode, "availableModes":options.iter().map(|(id,name,description)|
            json!({"id":id,"name":name,"description":description})).collect::<Vec<_>>()},
        "configOptions": [{"id":"mode","name":"Session Mode","category":"mode","type":"select",
            "currentValue":mode,"options":options.iter().map(|(value,name,description)|
                json!({"value":value,"name":name,"description":description})).collect::<Vec<_>>()}]
    })
}

pub(super) fn mode_updates(session: SessionId, mode: &PermissionMode) -> [Value; 2] {
    [
        json!({"sessionUpdate":"current_mode_update","currentModeId":mode}),
        json!({"sessionUpdate":"config_option_update","configOptions":mode_state(mode)["configOptions"]}),
    ].map(|update| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListSessions {
    pub cwd: Option<String>,
    pub cursor: Option<String>,
}

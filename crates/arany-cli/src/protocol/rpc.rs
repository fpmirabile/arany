use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};

pub const MAX_LINE_BYTES: usize = 1 << 20;

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq, Default)]
pub enum Version {
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Integer(#[schemars(range(min = i64::MIN, max = i64::MAX))] i64),
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: Version,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Notification {
    #[serde(default)]
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: Version,
    pub id: RequestId,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub result: Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorResponse {
    #[serde(default)]
    pub jsonrpc: Version,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    #[schemars(required, schema_with = "error_id_schema")]
    pub id: Option<RequestId>,
    pub error: ErrorObject,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorData {
    pub reason: Reason,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    NotInitialized,
    AlreadyInitialized,
    MessageTooLarge,
    Busy,
    ThreadNotLoaded,
    ThreadLimit,
    TurnNotActive,
    ServerShuttingDown,
    InvalidParams,
    UnsupportedPermissionMode,
    RunFull,
    Empty,
    TooLarge,
    Unavailable,
    IncompatibleProviderChain,
    InvalidRequest,
    MissingSession,
    NoSessionForWorkspace,
    TooManySessions,
    SessionBusy,
    WorkspaceBusy,
    GoalUnavailable,
    GoalAdmissionUncertain,
    GoalBudgetExhausted,
    GuardUnavailable,
    SessionNotIdle,
    ForkNotAtRunBoundary,
    WorkspaceMismatch,
    InvalidWorkspacePath,
    InputNotRegular,
    InputTooLarge,
    InputNotUtf8,
    MissingInclude,
    TooManyIncludes,
    StateOverlap,
    WorkspaceUnavailable,
    InvalidHistory,
    ContextTooLarge,
    ImagesNotAdmitted,
    CompactionInputEmpty,
    CompactionInputTooLarge,
    CompactionNotIdle,
    CancellationDrainTimeout,
    CoordinatorFailed,
    CancelledBeforeStart,
    Store,
    Tool,
    ProviderUnavailable,
    ProviderRejected,
    InvalidSelection,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
    Error(ErrorResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("invalid protocol message")]
    Parse,
    #[error("protocol message exceeds 1 MiB")]
    TooLarge,
}

pub fn encode_line(message: &impl Serialize) -> Result<Vec<u8>, CodecError> {
    struct BoundedLine {
        bytes: Vec<u8>,
        full: bool,
    }
    impl Write for BoundedLine {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_LINE_BYTES - 1 - self.bytes.len() {
                self.full = true;
                return Err(io::Error::other("protocol message limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut line = BoundedLine {
        bytes: Vec::new(),
        full: false,
    };
    serde_json::to_writer(&mut line, message).map_err(|_| {
        if line.full {
            CodecError::TooLarge
        } else {
            CodecError::Parse
        }
    })?;
    line.bytes.push(b'\n');
    Ok(line.bytes)
}

pub fn decode_line(line: &[u8]) -> Result<Message, CodecError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::TooLarge);
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.contains(&b'\n') {
        return Err(CodecError::Parse);
    }
    serde_json::from_slice(line).map_err(|_| CodecError::Parse)
}

fn error_id_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    generator.subschema_for::<Option<RequestId>>()
}

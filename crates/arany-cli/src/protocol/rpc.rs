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
    GoalNotContinuable,
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
    CompactionBlockedByHook,
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
    PreviewExpired,
    RewindBlocked,
    RewindTooLarge,
    ConversationRewindLimit,
    OverwriteConfirmationRequired,
    HumanConfirmationRequired,
}

impl From<&arany::EngineError> for Reason {
    fn from(error: &arany::EngineError) -> Self {
        match error {
            arany::EngineError::IncompatibleProviderChain => Reason::IncompatibleProviderChain,
            arany::EngineError::ConversationRewindLimit => Reason::ConversationRewindLimit,
            arany::EngineError::RewindBlocked => Reason::RewindBlocked,
            arany::EngineError::RewindPreviewExpired => Reason::PreviewExpired,
            arany::EngineError::RewindTooLarge => Reason::RewindTooLarge,
            arany::EngineError::RosterSelectionMismatch | arany::EngineError::InvalidRequest => {
                Reason::InvalidRequest
            }
            arany::EngineError::MissingSession => Reason::MissingSession,
            arany::EngineError::NoSessionForWorkspace => Reason::NoSessionForWorkspace,
            arany::EngineError::TooManySessions => Reason::TooManySessions,
            arany::EngineError::SessionBusy => Reason::SessionBusy,
            arany::EngineError::WorkspaceBusy => Reason::WorkspaceBusy,
            arany::EngineError::GoalUnavailable => Reason::GoalUnavailable,
            arany::EngineError::GoalAdmissionUncertain => Reason::GoalAdmissionUncertain,
            arany::EngineError::GoalBudgetExhausted => Reason::GoalBudgetExhausted,
            arany::EngineError::GoalNotContinuable => Reason::GoalNotContinuable,
            arany::EngineError::Busy => Reason::Busy,
            arany::EngineError::GuardUnavailable => Reason::GuardUnavailable,
            arany::EngineError::SessionNotIdle => Reason::SessionNotIdle,
            arany::EngineError::ForkNotAtRunBoundary => Reason::ForkNotAtRunBoundary,
            arany::EngineError::WorkspaceMismatch => Reason::WorkspaceMismatch,
            arany::EngineError::InvalidWorkspacePath => Reason::InvalidWorkspacePath,
            arany::EngineError::InputNotRegular => Reason::InputNotRegular,
            arany::EngineError::InputTooLarge => Reason::InputTooLarge,
            arany::EngineError::InputNotUtf8 => Reason::InputNotUtf8,
            arany::EngineError::MissingInclude => Reason::MissingInclude,
            arany::EngineError::TooManyIncludes => Reason::TooManyIncludes,
            arany::EngineError::StateOverlap => Reason::StateOverlap,
            arany::EngineError::WorkspaceUnavailable => Reason::WorkspaceUnavailable,
            arany::EngineError::InvalidHistory => Reason::InvalidHistory,
            arany::EngineError::ContextTooLarge => Reason::ContextTooLarge,
            arany::EngineError::ImagesNotAdmitted => Reason::ImagesNotAdmitted,
            arany::EngineError::CompactionInputEmpty => Reason::CompactionInputEmpty,
            arany::EngineError::CompactionBlockedByHook => Reason::CompactionBlockedByHook,
            arany::EngineError::CompactionInputTooLarge => Reason::CompactionInputTooLarge,
            arany::EngineError::CompactionNotIdle => Reason::CompactionNotIdle,
            arany::EngineError::CancellationDrainTimeout => Reason::CancellationDrainTimeout,
            arany::EngineError::CoordinatorFailed => Reason::CoordinatorFailed,
            arany::EngineError::CancelledBeforeStart => Reason::CancelledBeforeStart,
            arany::EngineError::Store(_) => Reason::Store,
            arany::EngineError::Tool(_) => Reason::Tool,
        }
    }
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

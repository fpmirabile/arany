use arany::{
    Effort, GoalId, MAX_ENCODED_IMAGE_BYTES, MAX_USER_MESSAGE_BYTES, RunId, SessionId,
    ToolDisposition,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::Value;

pub const CAPABILITIES: &[&str] = &[
    "threads_v1",
    "turns_v1",
    "items_v1",
    "interrupt_receipt_v1",
    "approvals_v2",
    "events_subscribe_v1",
    "receipt_get_v1",
    "thread_fork_v1",
    "turn_steer_v1",
    "goal_pause_v1",
    "thread_settings_update_v1",
    "thread_unsubscribe_v1",
    "thread_list_v1",
    "thread_rewind_v1",
];

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRewindPreviewParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RewindRestore {
    Code,
    Conversation,
    Both,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRewindParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[serde(deserialize_with = "client_string")]
    #[schemars(length(max = 256))]
    pub preview_token: String,
    pub restore: RewindRestore,
    #[serde(default, deserialize_with = "rewind_paths")]
    #[schemars(length(max = 64))]
    pub kept: Vec<String>,
    #[serde(default, deserialize_with = "rewind_paths")]
    #[schemars(length(max = 64))]
    pub overwrite: Vec<String>,
    #[serde(default)]
    pub confirm_overwrite: bool,
}

fn rewind_paths<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let paths = Vec::<String>::deserialize(deserializer)?;
    if paths.len() > 64
        || paths.iter().any(|p| p.is_empty() || p.len() > 512)
        || paths.windows(2).any(|p| p[0] >= p[1])
    {
        return Err(D::Error::custom("invalid rewind selections"));
    }
    Ok(paths)
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewindFilePreview {
    pub path: String,
    pub state: RewindFileState,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RewindFileState {
    Restore,
    Delete,
    Recreate,
    Drifted,
    HistoryDrift,
    Unavailable,
    Unsafe,
    OutsideGrant,
    Kept,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RewindBlocker {
    UncertainPublication,
    NoWriteGrant,
    GuardUnavailable,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewindNonRewindable {
    pub sequence: u64,
    pub effect: RewindEffect,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RewindEffect {
    Host,
    UnguardedCommand,
    Mcp,
    Cache,
    Directory,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRewindPreviewResult {
    pub checkpoint: RewindCheckpoint,
    pub restore: Vec<RewindRestore>,
    #[schemars(length(max = 2048))]
    pub files: Vec<RewindFilePreview>,
    pub blockers: Vec<RewindBlocker>,
    pub non_rewindable: Vec<RewindNonRewindable>,
    pub preview_token: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewindCheckpoint {
    pub kind: RewindCheckpointKind,
    pub id: String,
    pub sequence: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RewindCheckpointKind {
    Run,
    Rewind,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadRewindResult {
    pub rewind: RewindReceipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<RewindConversation>,
    pub notices: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewindReceipt {
    pub id: Option<String>,
    pub outcome: RewindResultOutcome,
    #[schemars(length(max = 32))]
    pub conflicts: Vec<String>,
    pub conflict_count: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RewindResultOutcome {
    Restored,
    Conflict,
    Denied,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewindConversation {
    #[schemars(with = "Option<String>")]
    pub thread_id: Option<SessionId>,
    pub prompt: Option<String>,
    pub images_omitted: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ClientCapabilities>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientInfo {
    #[serde(deserialize_with = "client_string")]
    #[schemars(length(max = 256))]
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_client_string"
    )]
    #[schemars(length(max = 256))]
    pub title: Option<String>,
    #[serde(deserialize_with = "client_string")]
    #[schemars(length(max = 256))]
    pub version: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "notification_methods"
    )]
    #[schemars(length(max = 32))]
    pub opt_out_notification_methods: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeResult {
    pub user_agent: String,
    pub platform_os: PlatformOs,
    pub capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PlatformOs {
    Linux,
    Macos,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadStartParams {
    #[serde(deserialize_with = "absolute_path")]
    #[schemars(regex(pattern = "^/"))]
    pub cwd: String,
    #[schemars(regex(pattern = "^(openai|anthropic|chatgpt|custom:.+)$"))]
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_effort_schema")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration: Option<Collaboration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_children: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "include_paths"
    )]
    pub include_paths: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadResumeParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[serde(deserialize_with = "absolute_path")]
    #[schemars(regex(pattern = "^/"))]
    pub cwd: String,
    #[schemars(regex(pattern = "^(openai|anthropic|chatgpt|custom:.+)$"))]
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_effort_schema")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration: Option<Collaboration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_children: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "include_paths"
    )]
    pub include_paths: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Collaboration {
    Single,
    Auto,
    Team,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Auto,
}

pub fn deserialize_permission_mode<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<PermissionMode, D::Error> {
    let value = String::deserialize(deserializer)?;
    serde_json::from_value(Value::String(value)).map_err(D::Error::custom)
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSettingsUpdateParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[serde(deserialize_with = "deserialize_permission_mode")]
    pub permission_mode: PermissionMode,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSettingsUpdateResult {
    pub permission_mode: PermissionMode,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadUnsubscribeParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadUnsubscribeResult {
    pub status: ThreadUnsubscribeStatus,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ThreadUnsubscribeStatus {
    NotLoaded,
    Unsubscribed,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadListResult {
    #[schemars(length(max = 100))]
    pub data: Vec<ThreadListEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadListEntry {
    #[schemars(with = "String")]
    pub id: SessionId,
    pub title: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadResult {
    pub thread: Thread,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadResumeResult {
    pub thread: Thread,
    pub turns: Vec<Turn>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Thread {
    #[schemars(with = "String")]
    pub id: SessionId,
    pub title: String,
    pub workspace: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub forked_from_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStartParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[serde(deserialize_with = "turn_input")]
    #[schemars(length(min = 1, max = 1 + arany::MAX_MESSAGE_IMAGES))]
    pub input: Vec<UserInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalInput>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UserInput {
    Text {
        #[schemars(length(max = MAX_USER_MESSAGE_BYTES))]
        text: String,
    },
    Image {
        media_type: ImageMediaType,
        #[schemars(length(max = MAX_ENCODED_IMAGE_BYTES))]
        data: String,
    },
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TextInput {
    Text {
        #[schemars(length(max = MAX_USER_MESSAGE_BYTES))]
        text: String,
    },
}

impl From<TextInput> for UserInput {
    fn from(TextInput::Text { text }: TextInput) -> Self {
        UserInput::Text { text }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub enum ImageMediaType {
    #[serde(rename = "image/png")]
    Png,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalInput {
    pub autonomous: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub resume_goal_id: Option<GoalId>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnInterruptParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnSteerParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
    #[serde(deserialize_with = "text_input")]
    #[schemars(length(min = 1, max = 1))]
    pub input: Vec<TextInput>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnSteerResult {
    #[schemars(with = "String")]
    pub turn_id: RunId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalPauseParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub goal_id: GoalId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalResumeParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub goal_id: GoalId,
    #[serde(default, deserialize_with = "optional_text_input")]
    #[schemars(length(min = 1, max = 1))]
    pub input: Option<Vec<TextInput>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnResult {
    pub turn: Turn,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Turn {
    #[schemars(with = "String")]
    pub id: RunId,
    #[schemars(with = "String")]
    pub goal_id: GoalId,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<TurnStopReason>,
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStopReason {
    ActionBudgetExhausted,
    TokenBudgetExhausted,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnError {
    pub message: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalSummary {
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<GoalStop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "budget_refusal_schema")]
    pub budget_refusal: Option<arany::BudgetRefusal>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(
    tag = "reason",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum GoalStop {
    GoalExhausted {
        limit: GoalLimit,
    },
    Paused,
    Cancelled,
    StoppedByHook,
    Uncertain,
    Interrupted,
    /// The next autonomous Run was refused after this one committed; `turn.error` has the message.
    NotAdmitted {
        refusal: super::rpc::Reason,
    },
    Failed {
        #[schemars(schema_with = "run_failure_schema")]
        failure: arany::RunFailure,
    },
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GoalLimit {
    Calls,
    Runs,
    ActiveTime,
    Tokens,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum GoalStatus {
    Running,
    Interrupted,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Item {
    UserMessage {
        id: String,
        text: String,
        images: Vec<ImageMetadata>,
    },
    AgentMessage {
        id: String,
        text: String,
        phase: AgentMessagePhase,
    },
    CommandExecution {
        id: String,
        command: String,
        args: Vec<String>,
        cwd: String,
        status: ItemStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(schema_with = "optional_disposition_schema")]
        disposition: Option<ToolDisposition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    FileChange {
        id: String,
        changes: Vec<FileChange>,
        status: ItemStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(schema_with = "optional_disposition_schema")]
        disposition: Option<ToolDisposition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    McpToolCall {
        id: String,
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
        status: ItemStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(schema_with = "optional_disposition_schema")]
        disposition: Option<ToolDisposition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    ToolCall {
        id: String,
        #[schemars(with = "serde_json::Map<String, Value>")]
        call: Value,
        status: ItemStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(schema_with = "optional_disposition_schema")]
        disposition: Option<ToolDisposition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Self::UserMessage { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileChange { id, .. }
            | Self::McpToolCall { id, .. }
            | Self::ToolCall { id, .. } => id,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageMetadata {
    pub index: usize,
    pub media_type: ImageMediaType,
    pub bytes: usize,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub enum AgentMessagePhase {
    #[serde(rename = "final_answer")]
    FinalAnswer,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Uncertain,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Attestation {
    Guard,
    InProcess,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeKind {
    Add,
    Update,
    Delete,
    Mkdir,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnNotification {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    pub turn: Turn,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemStartedNotification {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
    pub item: Item,
    pub started_at_ms: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemCompletedNotification {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
    pub item: Item,
    pub completed_at_ms: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsResyncNotification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub thread_id: Option<SessionId>,
    pub missed: u64,
    pub last_sequence: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub turn_id: RunId,
    pub item_id: String,
    pub started_at_ms: i64,
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub intent: Value,
    pub lines: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalResult {
    pub decision: ApprovalDecision,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

pub const MAX_EVENT_PAGE: usize = 1_000;

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsSubscribeParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    pub since: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsSubscribeResult {
    #[schemars(length(max = MAX_EVENT_PAGE))]
    pub events: Vec<super::EventLine>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    #[schemars(required, schema_with = "nullable_sequence_schema")]
    pub next_since: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsCommittedNotification {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    pub event: super::EventLine,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptGetParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[schemars(with = "String")]
    pub goal_id: GoalId,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptGetResult {
    pub goal: ReceiptGoal,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptGoal {
    #[schemars(with = "String")]
    pub id: GoalId,
    pub status: GoalStatus,
    pub objective: String,
    pub budget: ReceiptBudget,
    pub usage: ReceiptUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ReceiptReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "budget_refusal_schema")]
    pub budget_refusal: Option<arany::BudgetRefusal>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptBudget {
    pub max_provider_calls: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_active_ms: Option<u64>,
    pub max_runs: u8,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptUsage {
    pub runs: usize,
    pub provider_calls: u32,
    pub active_ms: u64,
    pub uncertain_active_ms: u64,
    pub known_tokens: u64,
    pub known_cached_tokens: u64,
    pub token_usage_unknown: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptReport {
    pub summary: String,
    pub changes: Vec<String>,
    pub evidence: Vec<String>,
    pub unresolved: Vec<String>,
    pub cached_input_tokens: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadForkParams {
    #[schemars(with = "String")]
    pub thread_id: SessionId,
    #[serde(deserialize_with = "absolute_path")]
    #[schemars(regex(pattern = "^/"))]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub title: Option<String>,
}

fn nullable_sequence_schema(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<Option<u64>>()
}

fn client_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() > 256 {
        return Err(D::Error::custom("clientInfo string exceeds 256 bytes"));
    }
    Ok(value)
}

fn optional_client_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|value| value.len() > 256) {
        return Err(D::Error::custom("clientInfo string exceeds 256 bytes"));
    }
    Ok(value)
}

fn notification_methods<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|value| value.len() > 32) {
        return Err(D::Error::custom("at most 32 notification opt-outs"));
    }
    Ok(value)
}

fn absolute_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if !std::path::Path::new(&value).is_absolute() || value.contains('\0') {
        return Err(D::Error::custom("path must be absolute"));
    }
    Ok(value)
}

fn include_paths<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|paths| {
        paths
            .iter()
            .any(|path| !std::path::Path::new(path).is_absolute() || path.contains('\0'))
    }) {
        return Err(D::Error::custom("include paths must be absolute"));
    }
    Ok(value)
}

fn turn_input<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<UserInput>, D::Error> {
    use base64::Engine as _;
    let input = Vec::<UserInput>::deserialize(deserializer)?;
    let mut texts = 0;
    let mut images = 0;
    let mut image_bytes = 0;
    for item in &input {
        match item {
            UserInput::Text { text } => {
                texts += 1;
                if text.len() > MAX_USER_MESSAGE_BYTES {
                    return Err(D::Error::custom(format!(
                        "text exceeds {} KiB",
                        MAX_USER_MESSAGE_BYTES / 1024
                    )));
                }
            }
            UserInput::Image { data, .. } => {
                images += 1;
                if data.len() > MAX_ENCODED_IMAGE_BYTES {
                    return Err(D::Error::custom("image exceeds byte limit"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| D::Error::custom("invalid image base64"))?;
                image_bytes += bytes.len();
                if image_bytes > arany::MAX_IMAGE_BYTES {
                    return Err(D::Error::custom("images exceed total byte limit"));
                }
            }
        }
    }
    if input.is_empty() || texts > 1 || images > arany::MAX_MESSAGE_IMAGES {
        return Err(D::Error::custom(format!(
            "at most one text and {} images",
            arany::MAX_MESSAGE_IMAGES
        )));
    }
    Ok(input)
}

fn single_text<E: Error>(input: Vec<TextInput>) -> Result<Vec<TextInput>, E> {
    match input.as_slice() {
        [TextInput::Text { text }] if text.len() <= MAX_USER_MESSAGE_BYTES => Ok(input),
        _ => Err(E::custom(format!(
            "exactly one text of at most {} KiB",
            MAX_USER_MESSAGE_BYTES / 1024
        ))),
    }
}

fn text_input<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<TextInput>, D::Error> {
    single_text(Vec::deserialize(deserializer)?)
}

fn optional_text_input<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<TextInput>>, D::Error> {
    Option::deserialize(deserializer)?
        .map(single_text)
        .transpose()
}

fn optional_effort_schema(_: &mut SchemaGenerator) -> Schema {
    let mut values: Vec<Value> = Effort::ALL
        .iter()
        .map(|effort| Value::from(effort.as_str()))
        .collect();
    values.push(Value::Null);
    json_schema!({"enum": values})
}

fn optional_disposition_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"enum": ["succeeded", "denied", "failed", "conflict", "limit", "cancelled", "uncertain", null]})
}

fn run_failure_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"enum": ["provider_call", "proposal_refused", "budget_exhausted", "tool_stopped", "definition_invalid", "plan_admission", "tool_context_full", "workers_unverified", "criterion_unmet", "refusal_limit"], "type": "string"})
}

fn budget_refusal_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["object", "null"], "additionalProperties": false,
        "required": ["dimension", "cause", "action", "run_id", "agent_run_id", "used", "limit", "requested", "required_minimum", "token_usage_unknown", "charge_basis"],
        "properties": {
            "dimension": {"enum": ["tokens", "calls", "active_time", "runs"]},
            "cause": {"enum": ["exhausted", "unknown_usage", "insufficient_headroom"]},
            "action": {"enum": ["provider", "tool", "delegation", "compaction", "run", "resume"]},
            "run_id": {"type": ["string", "null"]}, "agent_run_id": {"type": ["string", "null"]},
            "used": {"type": "integer", "minimum": 0}, "limit": {"type": "integer", "minimum": 1},
            "requested": {"type": ["integer", "null"], "minimum": 0},
            "required_minimum": {"type": ["integer", "null"], "minimum": 0},
            "token_usage_unknown": {"type": "boolean"},
            "charge_basis": {"enum": ["observed", "conservative"]}
        }
    })
}

// Keep these exhaustive matches and the serialized-variant schema test in sync.
const _: fn(arany::GoalLimit) = |limit| match limit {
    arany::GoalLimit::Calls
    | arany::GoalLimit::Tokens
    | arany::GoalLimit::ActiveTime
    | arany::GoalLimit::Runs => {}
};
const _: fn(arany::BudgetRefusalCause) = |cause| match cause {
    arany::BudgetRefusalCause::Exhausted
    | arany::BudgetRefusalCause::UnknownUsage
    | arany::BudgetRefusalCause::InsufficientHeadroom => {}
};
const _: fn(arany::BudgetAction) = |action| match action {
    arany::BudgetAction::Provider
    | arany::BudgetAction::Tool
    | arany::BudgetAction::Delegation
    | arany::BudgetAction::Compaction
    | arany::BudgetAction::Run
    | arany::BudgetAction::Resume => {}
};
const _: fn(arany::BudgetChargeBasis) = |basis| match basis {
    arany::BudgetChargeBasis::Observed | arany::BudgetChargeBasis::Conservative => {}
};
const _: fn(arany::RunFailure) = |failure| match failure {
    arany::RunFailure::ProviderCall
    | arany::RunFailure::ProposalRefused
    | arany::RunFailure::BudgetExhausted
    | arany::RunFailure::ToolStopped
    | arany::RunFailure::DefinitionInvalid
    | arany::RunFailure::PlanAdmission
    | arany::RunFailure::ToolContextFull
    | arany::RunFailure::WorkersUnverified
    | arany::RunFailure::CriterionUnmet
    | arany::RunFailure::RefusalLimit => {}
};

// A new ToolDisposition variant fails this match until the schema enum above lists it.
const _: fn(arany::ToolDisposition) = |disposition| match disposition {
    arany::ToolDisposition::Succeeded
    | arany::ToolDisposition::Denied
    | arany::ToolDisposition::Failed
    | arany::ToolDisposition::Conflict
    | arany::ToolDisposition::Limit
    | arany::ToolDisposition::Cancelled
    | arany::ToolDisposition::Uncertain => {}
};

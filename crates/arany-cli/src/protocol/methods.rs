use arany::{Effort, GoalId, RunId, SessionId, ToolDisposition};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use serde_json::Value;

const MAX_ENCODED_IMAGE_BYTES: usize = arany::MAX_IMAGE_BYTES.div_ceil(3) * 4;

pub const CAPABILITIES: &[&str] = &[
    "threads_v1",
    "turns_v1",
    "items_v1",
    "interrupt_receipt_v1",
    "approvals_v1",
    "turn_steer_v1",
    "goal_pause_v1",
];

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
        #[schemars(length(max = 65536))]
        text: String,
    },
    Image {
        media_type: ImageMediaType,
        #[schemars(length(max = MAX_ENCODED_IMAGE_BYTES))]
        data: String,
    },
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
    pub input: Vec<UserInput>,
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
    pub input: Option<Vec<UserInput>>,
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
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSummary>,
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
    pub stopped: Option<String>,
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
    pub expires_at_ms: i64,
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
                if text.len() > 64 * 1024 {
                    return Err(D::Error::custom("text exceeds 64 KiB"));
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

fn single_text<E: Error>(input: Vec<UserInput>) -> Result<Vec<UserInput>, E> {
    match input.as_slice() {
        [UserInput::Text { text }] if text.len() <= 64 * 1024 => Ok(input),
        _ => Err(E::custom("exactly one text of at most 64 KiB")),
    }
}

fn text_input<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<UserInput>, D::Error> {
    single_text(Vec::deserialize(deserializer)?)
}

fn optional_text_input<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<UserInput>>, D::Error> {
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

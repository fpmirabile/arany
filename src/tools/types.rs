use crate::session::{AgentRunId, RunId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use uuid::Uuid;

pub const MAX_TOOL_CALLS: usize = 16;
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_CONTEXT_BYTES: usize = 64 * 1024;
pub const MAX_MODEL_STEPS: usize = 32;
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_FILES: usize = 2048;
pub(crate) const MAX_RUNTIME_BYTES: usize = 96 * 1024 * 1024;
pub(crate) const SCRATCH_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MEMORY_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_PROCESSES: u32 = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolCall {
    List {
        path: String,
    },
    Read {
        path: String,
        offset: u32,
        limit: u32,
    },
    Search {
        path: String,
        query: String,
    },
    Mkdir {
        path: String,
    },
    Write {
        path: String,
        #[serde(deserialize_with = "required_nullable")]
        expected_digest: Option<String>,
        content: String,
    },
    Edit {
        path: String,
        expected_digest: String,
        old: String,
        new: String,
    },
    Command {
        program: String,
        args: Vec<String>,
        cwd: String,
    },
    Skill {
        name: String,
        #[serde(deserialize_with = "required_nullable")]
        resource: Option<String>,
    },
    McpList {
        server: String,
    },
    McpCall {
        server: String,
        tool: String,
        schema_digest: String,
        arguments: String,
    },
}

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

pub(crate) fn parse_mcp_arguments(arguments: &str) -> Result<Value, super::ToolError> {
    let value = super::skills::parse_json(arguments.as_bytes(), 8 * 1024)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(super::ToolError::Operation)
    }
}

impl ToolCall {
    pub(crate) fn valid(&self) -> bool {
        if !serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_ARGUMENT_BYTES) {
            return false;
        }
        match self {
            Self::List { path } => valid_relative(path, true),
            Self::Read { path, limit, .. } => {
                valid_relative(path, false) && (1..=4096).contains(limit)
            }
            Self::Search { path, query } => {
                valid_relative(path, true) && !query.is_empty() && query.len() <= 512
            }
            Self::Mkdir { path } => valid_relative(path, false),
            Self::Write {
                path,
                expected_digest,
                content,
            } => {
                valid_relative(path, false)
                    && content.len() <= 8 * 1024
                    && expected_digest
                        .as_ref()
                        .is_none_or(|digest| valid_digest(digest))
            }
            Self::Edit {
                path,
                expected_digest,
                old,
                new,
            } => {
                valid_relative(path, false)
                    && valid_digest(expected_digest)
                    && !old.is_empty()
                    && old.len() + new.len() <= 8 * 1024
            }
            Self::Command { program, args, cwd } => {
                valid_name(program)
                    && valid_relative(cwd, true)
                    && args.len() <= 64
                    && args
                        .iter()
                        .all(|arg| arg.len() <= 4096 && !arg.contains('\0'))
            }
            Self::Skill { name, resource } => {
                valid_name(name)
                    && resource
                        .as_ref()
                        .is_none_or(|path| valid_relative(path, false))
            }
            Self::McpList { server } => valid_name(server),
            Self::McpCall {
                server,
                tool,
                schema_digest,
                arguments,
            } => {
                valid_name(server)
                    && valid_digest(schema_digest)
                    && !tool.is_empty()
                    && tool.len() <= 128
                    && tool.chars().all(|ch| !ch.is_control())
                    && parse_mcp_arguments(arguments).is_ok()
            }
        }
    }

    pub(crate) fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Write { .. } | Self::Edit { .. } | Self::Mkdir { .. }
        )
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Read { .. } => "read",
            Self::Search { .. } => "search",
            Self::Mkdir { .. } => "mkdir",
            Self::Write { .. } => "write",
            Self::Edit { .. } => "edit",
            Self::Command { .. } => "command",
            Self::Skill { .. } => "skill",
            Self::McpList { .. } => "mcp_list",
            Self::McpCall { .. } => "mcp_call",
        }
    }
}

pub(crate) fn valid_relative(value: &str, root: bool) -> bool {
    if value.is_empty() || value == "." {
        return root;
    }
    value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !forbidden(part))
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn forbidden(value: &str) -> bool {
    matches!(
        value,
        ".git" | ".aws" | ".ssh" | ".gnupg" | ".env" | ".npmrc" | ".pypirc"
    ) || value.starts_with(".env.")
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyReceipt {
    pub contract_version: u32,
    pub config_digest: [u8; 32],
    pub max_tool_calls: u32,
    pub max_model_steps: u32,
    pub max_context_bytes: u32,
}

impl ToolPolicyReceipt {
    pub(crate) fn valid(&self) -> bool {
        self.contract_version == 1
            && self.config_digest != [0; 32]
            && self.max_tool_calls == MAX_TOOL_CALLS as u32
            && self.max_model_steps == MAX_MODEL_STEPS as u32
            && self.max_context_bytes == MAX_TOOL_CONTEXT_BYTES as u32
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectIntent {
    pub id: Uuid,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub policy_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub call: ToolCall,
    pub limits: ToolLimits,
    pub expires_at_ms: u64,
    pub use_count: u8,
}

impl EffectIntent {
    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed intent is serializable"))
    }

    pub(crate) fn valid(&self) -> bool {
        self.id.get_version() == Some(uuid::Version::SortRand)
            && self.policy_digest != [0; 32]
            && self.enforcement_digest != [0; 32]
            && self.workspace_inode > 0
            && self.expires_at_ms > 0
            && self.use_count == 1
            && self.call.valid()
            && self.limits.valid()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLimits {
    pub result_bytes: u32,
    pub runtime_ms: u32,
    pub memory_bytes: u64,
    pub max_processes: u32,
    pub scratch_bytes: u64,
    pub network: NetworkGrant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkGrant {
    None,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            result_bytes: MAX_TOOL_RESULT_BYTES as u32,
            runtime_ms: 60_000,
            memory_bytes: MEMORY_BYTES,
            max_processes: MAX_PROCESSES,
            scratch_bytes: SCRATCH_BYTES,
            network: NetworkGrant::None,
        }
    }
}

impl ToolLimits {
    fn valid(&self) -> bool {
        (128..=MAX_TOOL_RESULT_BYTES as u32).contains(&self.result_bytes)
            && (1_000..=60_000).contains(&self.runtime_ms)
            && self.memory_bytes == MEMORY_BYTES
            && self.max_processes == MAX_PROCESSES
            && self.scratch_bytes == SCRATCH_BYTES
            && self.network == NetworkGrant::None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDisposition {
    Succeeded,
    Denied,
    Failed,
    Conflict,
    Limit,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardReceipt {
    pub contract_version: u32,
    pub intent_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub limits: ToolLimits,
}

impl GuardReceipt {
    pub(crate) fn valid_for(&self, intent: &EffectIntent) -> bool {
        self.contract_version == 1
            && self.intent_digest == intent.digest()
            && self.enforcement_digest == intent.enforcement_digest
            && self.limits == intent.limits
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolObservation {
    pub intent: EffectIntent,
    pub disposition: ToolDisposition,
    pub output: String,
    pub guard: Option<GuardReceipt>,
}

impl ToolObservation {
    pub(crate) fn stops_run(&self) -> bool {
        matches!(
            self.disposition,
            ToolDisposition::Uncertain | ToolDisposition::Cancelled
        ) || (self.disposition == ToolDisposition::Failed && self.guard.is_none())
    }

    pub(crate) fn valid(&self) -> bool {
        self.intent.valid()
            && self.output.len() <= self.intent.limits.result_bytes as usize
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= 48 * 1024)
            && self
                .guard
                .as_ref()
                .is_none_or(|guard| guard.valid_for(&self.intent))
            && (self.disposition != ToolDisposition::Succeeded || self.guard.is_some())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolContext {
    pub catalog: String,
    pub observations: Vec<ToolObservation>,
}

impl ToolContext {
    pub(crate) fn model_input(&self) -> Value {
        let catalog = super::skills::parse_json(self.catalog.as_bytes(), 8 * 1024)
            .unwrap_or_else(|_| json!(self.catalog));
        let observations: Vec<_> = self
            .observations
            .iter()
            .map(|observation| {
                let output = super::skills::parse_json(
                    observation.output.as_bytes(),
                    MAX_TOOL_RESULT_BYTES,
                )
                .unwrap_or_else(|_| json!(observation.output));
                let effect = if !observation.intent.call.mutates() {
                    "none"
                } else {
                    match observation.disposition {
                        ToolDisposition::Succeeded if observation.guard.as_ref().is_some_and(|guard| guard.valid_for(&observation.intent)) => "applied",
                        ToolDisposition::Uncertain | ToolDisposition::Cancelled => "uncertain",
                        _ => "not_confirmed",
                    }
                };
                json!({"call":observation.intent.call,"disposition":observation.disposition,"output":output,"workspace_effect":effect})
            })
            .collect();
        json!({"catalog":catalog,"observations":observations})
    }

    pub(crate) fn outcome_branch(&self) -> Value {
        let catalog =
            super::skills::parse_json(self.catalog.as_bytes(), 8 * 1024).unwrap_or(Value::Null);
        let mut branch = outcome_branch();
        branch["properties"]["call"]["anyOf"]
            .as_array_mut()
            .expect("compiled Tool branches")
            .retain_mut(|call| {
                let (key, field, cap) = match call["properties"]["operation"]["enum"][0].as_str() {
                    Some("write" | "edit" | "mkdir") => return catalog["write"] == true,
                    Some("command") => ("commands", "program", 8),
                    Some("skill") => ("skills", "name", 32),
                    Some("mcp_list" | "mcp_call") => ("mcp_servers", "server", 4),
                    _ => return true,
                };
                let names: Vec<_> = catalog[key]
                    .as_array()
                    .filter(|rows| rows.len() <= cap)
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["name"].as_str())
                    .filter(|name| valid_name(name))
                    .collect();
                if names.is_empty() {
                    return false;
                }
                call["properties"][field]["enum"] = json!(names);
                true
            });
        branch
    }
}

pub(crate) fn outcome_branch() -> Value {
    let string = json!({"type": "string"});
    let nullable = json!({"type": ["string", "null"]});
    let definitions = [
        ("list", vec![("path", string.clone())]),
        ("mkdir", vec![("path", string.clone())]),
        (
            "read",
            vec![
                ("path", string.clone()),
                (
                    "offset",
                    json!({"type":"integer", "description":"Zero-based byte offset, 0 through 4294967295. Start at 0; page using the returned next offset."}),
                ),
                (
                    "limit",
                    json!({"type":"integer", "description":"Number of UTF-8 bytes to read: integer 1 through 4096 inclusive. Use 4096 for a normal page; never request the whole file with a larger limit."}),
                ),
            ],
        ),
        (
            "search",
            vec![("path", string.clone()), ("query", string.clone())],
        ),
        (
            "write",
            vec![
                ("path", string.clone()),
                ("expected_digest", nullable.clone()),
                ("content", string.clone()),
            ],
        ),
        (
            "edit",
            vec![
                ("path", string.clone()),
                (
                    "expected_digest",
                    json!({"type":"string", "description":"Exact lowercase 64-character SHA-256 from the file include or a successful Read. Never invent it."}),
                ),
                (
                    "old",
                    json!({"type":"string", "description":"Non-empty exact literal text occurring once in the current file. To append, select a unique ending and include that ending plus the appended text in new."}),
                ),
                (
                    "new",
                    json!({"type":"string", "description":"Replacement literal text. Combined old and new UTF-8 size must be at most 8192 bytes."}),
                ),
            ],
        ),
        (
            "command",
            vec![
                ("program", string.clone()),
                ("args", json!({"type":"array", "items": {"type":"string"}})),
                ("cwd", string.clone()),
            ],
        ),
        (
            "skill",
            vec![("name", string.clone()), ("resource", nullable)],
        ),
        ("mcp_list", vec![("server", string.clone())]),
        (
            "mcp_call",
            vec![
                ("server", string.clone()),
                ("tool", string.clone()),
                ("schema_digest", string.clone()),
                ("arguments", string),
            ],
        ),
    ];
    let branches: Vec<_> = definitions.into_iter().map(|(operation, fields)| {
        let mut properties = serde_json::Map::new();
        properties.insert("operation".into(), json!({"type":"string", "enum":[operation]}));
        let mut required = vec!["operation"];
        for (name, schema) in fields { properties.insert(name.into(), schema); required.push(name); }
        json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
    }).collect();
    json!({"type":"object", "properties": {"type":{"type":"string","enum":["tool"]}, "call":{"anyOf":branches}}, "required":["type","call"], "additionalProperties":false})
}

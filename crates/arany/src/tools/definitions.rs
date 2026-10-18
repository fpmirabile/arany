use super::config::Config;
use super::permissions::{Decision, PermissionMode};
use super::types::{ToolCall, digest, valid_name};
use super::{ToolError, fs};
use base64::Engine as _;
use cap_std::fs::{Dir, MetadataExt as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;

pub(crate) const MAX_DEFINITIONS: usize = 64;
const MAX_RAW_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionRegistryPin {
    pub digest: [u8; 32],
    pub files: Vec<DefinitionFilePin>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionFilePin {
    pub private: bool,
    pub filename: String,
    pub digest: [u8; 32],
}

impl DefinitionRegistryPin {
    pub(crate) fn valid(&self) -> bool {
        !self.files.is_empty()
            && self.files.len() <= 2 * MAX_DEFINITIONS
            && self.files.iter().all(|file| valid_filename(&file.filename))
            && self.files.windows(2).all(|pair| {
                (pair[0].private, &pair[0].filename) < (pair[1].private, &pair[1].filename)
            })
            && serde_json::to_vec(&self.files).is_ok_and(|bytes| digest(&bytes) == self.digest)
    }
}

#[derive(Clone, Default)]
pub(crate) struct DefinitionSnapshots {
    first_index: u16,
    pub(crate) files: Vec<(DefinitionFilePin, String)>,
}

impl DefinitionSnapshots {
    #[cfg(target_os = "macos")]
    pub(crate) fn from_payload(call: &ToolCall, payload: Option<String>) -> Self {
        if let (ToolCall::ParseAgentDefinition { index, raw_digest }, Some(payload)) =
            (call, payload)
        {
            return Self {
                first_index: *index,
                files: vec![(
                    DefinitionFilePin {
                        private: false,
                        filename: "snapshot.md".into(),
                        digest: *raw_digest,
                    },
                    payload,
                )],
            };
        }
        Self::default()
    }

    pub(crate) fn pin(&self) -> Option<DefinitionRegistryPin> {
        if self.files.is_empty() {
            return None;
        }
        let files: Vec<_> = self.files.iter().map(|(pin, _)| pin.clone()).collect();
        Some(DefinitionRegistryPin {
            digest: digest(&serde_json::to_vec(&files).expect("typed pins")),
            files,
        })
    }

    pub(crate) fn payload(&self, call: &ToolCall) -> Result<Option<String>, ToolError> {
        let ToolCall::ParseAgentDefinition { index, raw_digest } = call else {
            return Ok(None);
        };
        let (pin, bytes) = self
            .files
            .get(usize::from(
                index
                    .checked_sub(self.first_index)
                    .ok_or(ToolError::Configuration)?,
            ))
            .ok_or(ToolError::Configuration)?;
        if pin.digest != *raw_digest {
            return Err(ToolError::ChangedInput);
        }
        Ok(Some(bytes.clone()))
    }
}

pub(super) fn snapshot(
    workspace: &Dir,
    state: &crate::store::StateRoot,
    config: &Config,
) -> Result<DefinitionSnapshots, ToolError> {
    state.readmit().map_err(|_| ToolError::ChangedInput)?;
    let mut snapshots = DefinitionSnapshots::default();
    let mut bytes = 0;
    if config.in_workspace_scope(".claude/agents")
        && let Some(dir) = optional_directory(workspace, ".claude")?
        && let Some(agents) = optional_directory(&dir, "agents")?
    {
        scan(&agents, false, &mut snapshots, &mut bytes)?;
        if fs::identity(&agents)?
            != fs::identity(&Dir::from_std_file(fs::open_file(
                workspace,
                ".claude/agents",
            )?))?
        {
            return Err(ToolError::ChangedInput);
        }
    }
    let root =
        crate::private_fs::open_directory(state.path(), false).map_err(|_| ToolError::Path)?;
    if optional_directory(&root.dir, "agents")?.is_some() {
        let agents = crate::private_fs::open_directory(&state.path().join("agents"), false)
            .map_err(|_| ToolError::Path)?;
        scan(&agents.dir, true, &mut snapshots, &mut bytes)?;
        if fs::identity(&agents.dir)?
            != fs::identity(&Dir::from_std_file(fs::open_file(&root.dir, "agents")?))?
        {
            return Err(ToolError::ChangedInput);
        }
    }
    snapshots
        .files
        .sort_by(|a, b| (a.0.private, &a.0.filename).cmp(&(b.0.private, &b.0.filename)));
    state.readmit().map_err(|_| ToolError::ChangedInput)?;
    Ok(snapshots)
}

fn optional_directory(root: &Dir, name: &str) -> Result<Option<Dir>, ToolError> {
    match root.symlink_metadata(name) {
        Ok(_) => {
            let file = fs::open_file(root, name)?;
            if !file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
                return Err(ToolError::Path);
            }
            Ok(Some(Dir::from_std_file(file)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ToolError::Path),
    }
}

fn valid_filename(name: &str) -> bool {
    name.strip_suffix(".md").is_some_and(valid_name) && super::types::valid_relative(name, false)
}

fn scan(
    dir: &Dir,
    private: bool,
    snapshots: &mut DefinitionSnapshots,
    total: &mut usize,
) -> Result<(), ToolError> {
    let before_directory = dir.dir_metadata().map_err(|_| ToolError::Path)?;
    let identity = fs::identity(dir)?;
    let mut accepted = 0;
    for (index, entry) in dir.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if index == 256 {
            return Err(ToolError::Limit);
        }
        let name = entry
            .map_err(|_| ToolError::Path)?
            .file_name()
            .into_string()
            .map_err(|_| ToolError::Path)?;
        if !super::types::valid_relative(&name, false) {
            return Err(ToolError::Path);
        }
        let file = fs::open_file(dir, &name)?;
        let before = file.metadata().map_err(|_| ToolError::Path)?;
        if !before.is_file()
            || before.nlink() != 1
            || before.mode() & 0o6000 != 0
            || before.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(ToolError::Path);
        }
        if private {
            if before.mode() & 0o077 != 0 {
                return Err(ToolError::Path);
            }
            crate::private_fs::check_acl(&file, true).map_err(|_| ToolError::Path)?;
        }
        if !name.ends_with(".md") {
            continue;
        }
        if !valid_filename(&name) {
            return Err(ToolError::Configuration);
        }
        accepted += 1;
        if accepted > MAX_DEFINITIONS {
            return Err(ToolError::Limit);
        }
        let raw = fs::read_regular(file, 64 * 1024)?;
        *total += raw.len();
        if *total > MAX_RAW_BYTES {
            return Err(ToolError::Limit);
        }
        let after = dir
            .symlink_metadata(&name)
            .map_err(|_| ToolError::ChangedInput)?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) || !after.is_file()
            || after.nlink() != 1
        {
            return Err(ToolError::ChangedInput);
        }
        snapshots.files.push((
            DefinitionFilePin {
                private,
                filename: name,
                digest: digest(&raw),
            },
            base64::engine::general_purpose::STANDARD.encode(raw),
        ));
    }
    let after_directory = dir.dir_metadata().map_err(|_| ToolError::Path)?;
    if identity != fs::identity(dir)?
        || (
            before_directory.mtime(),
            before_directory.mtime_nsec(),
            before_directory.ctime(),
            before_directory.ctime_nsec(),
        ) != (
            after_directory.mtime(),
            after_directory.mtime_nsec(),
            after_directory.ctime(),
            after_directory.ctime_nsec(),
        )
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentDefinition {
    pub(crate) version: u8,
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) tools: Option<Vec<String>>,
    pub(crate) disallowed_tools: Vec<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) model: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) permission_mode: Option<PermissionMode>,
    pub(crate) max_turns: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) isolation: Option<String>,
    pub(crate) background: bool,
    pub(crate) body: String,
}

impl AgentDefinition {
    pub(crate) fn editor(&self) -> bool {
        self.isolation.is_some()
            || self.background
            || self.tools.as_ref().is_some_and(|tools| {
                tools.iter().any(|tool| {
                    matches!(tool.as_str(), "edit" | "write" | "mkdir")
                        && !self.disallowed_tools.contains(tool)
                })
            })
    }

    pub(crate) fn allows_worker(&self, call: &ToolCall) -> bool {
        crate::session::worker::WorkerTool::of(call)
            .is_some_and(|tool| self.allows_worker_tool(tool))
    }

    pub(crate) fn allows_worker_tool(&self, tool: crate::session::worker::WorkerTool) -> bool {
        if tool == crate::session::worker::WorkerTool::Command && !self.editor() {
            return false;
        }
        let label = tool.label();
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|name| name == label))
            && !self.disallowed_tools.iter().any(|name| name == label)
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        call.is_child_research()
            && self
                .tools
                .as_ref()
                .is_none_or(|tools| tools.iter().any(|name| name == call.label()))
            && !self
                .disallowed_tools
                .iter()
                .any(|name| name == call.label())
            && !(self.permission_mode == Some(PermissionMode::Plan) && call.is_process())
    }

    pub(crate) fn decision(&self, call: &ToolCall) -> Decision {
        if self.allows(call) || self.editor() && self.allows_worker(call) {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    pub(crate) fn valid(&self) -> bool {
        self.version == 1
            && valid_name(&self.name)
            && self.name.len() <= 64
            && !self.description.trim().is_empty()
            && self.description.len() <= 2048
            && self.body.len() <= 32 * 1024
            && (1..=crate::session::MAX_CHILD_TURNS).contains(&self.max_turns)
            && self.model.as_ref().is_none_or(|value| {
                !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
            })
            && self
                .isolation
                .as_deref()
                .is_none_or(|value| value == "worktree")
            && self
                .tools
                .iter()
                .chain(std::iter::once(&self.disallowed_tools))
                .all(|tools| {
                    tools.len() <= 32
                        && tools.windows(2).all(|pair| pair[0] < pair[1])
                        && tools.iter().all(|name| {
                            supported_tool(name).is_some_and(|mapped| mapped == [name.as_str()])
                        })
                        && tools.iter().collect::<BTreeSet<_>>().len() == tools.len()
                })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Frontmatter {
    name: String,
    description: String,
    tools: Option<serde_json::Value>,
    disallowed_tools: Option<serde_json::Value>,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    max_turns: Option<u8>,
    isolation: Option<String>,
    background: Option<bool>,
}

fn supported_tool(name: &str) -> Option<Vec<&str>> {
    Some(match name {
        "Read" => vec!["read"],
        "Glob" => vec!["list"],
        "Grep" => vec!["search"],
        "Edit" => vec!["edit"],
        "Write" => vec!["write"],
        "Skill" => vec!["skill"],
        "Bash" => vec!["command"],
        "list" | "read" | "search" | "edit" | "write" | "mkdir" | "skill" | "mcp_list"
        | "command" => {
            vec![name]
        }
        _ => return None,
    })
}

pub(crate) const MAX_AGENT_CATALOG_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTICS: usize = 64;

#[derive(Debug)]
pub(super) struct Rejected {
    field: &'static str,
    reason: &'static str,
}

impl From<ToolError> for Rejected {
    fn from(error: ToolError) -> Self {
        match error {
            ToolError::Limit => Rejected {
                field: "file",
                reason: "over a limit",
            },
            _ => Rejected {
                field: "frontmatter",
                reason: "invalid",
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ParsedFile {
    Definition {
        definition: AgentDefinition,
        notes: Vec<String>,
    },
    Skipped {
        diagnostic: String,
    },
}

fn tool_list(value: serde_json::Value, notes: &mut Vec<String>) -> Result<Vec<String>, Rejected> {
    let names: Vec<String> = match value {
        serde_json::Value::String(value) => value
            .split(',')
            .map(|name| name.trim().to_owned())
            .collect(),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(Rejected {
                field: "tools",
                reason: "entries must be strings",
            })?,
        _ => {
            return Err(Rejected {
                field: "tools",
                reason: "must be a list or comma separated string",
            });
        }
    };
    if names.len() > 32 || names.iter().any(|name| name.len() > 128) {
        return Err(Rejected {
            field: "tools",
            reason: "over a limit",
        });
    }
    let mut result = BTreeSet::new();
    let mut ignored = 0;
    for name in names {
        match supported_tool(&name) {
            Some(operations) => result.extend(operations.into_iter().map(str::to_owned)),
            None => ignored += 1,
        }
    }
    if ignored > 0 {
        notes.push("tools: unsupported names ignored".into());
    }
    Ok(result.into_iter().collect())
}

const KNOWN_FIELDS: [&str; 9] = [
    "name",
    "description",
    "tools",
    "disallowedTools",
    "model",
    "permissionMode",
    "maxTurns",
    "isolation",
    "background",
];

fn known(key: &str) -> &'static str {
    KNOWN_FIELDS
        .iter()
        .find(|field| **field == key)
        .copied()
        .unwrap_or("frontmatter")
}

pub(super) fn strict(raw: &[u8]) -> Result<(AgentDefinition, Vec<String>), Rejected> {
    let (value, body) = super::skills::definition_document(raw)?;
    let serde_json::Value::Object(mut map) = value else {
        return Err(Rejected {
            field: "frontmatter",
            reason: "must be a mapping",
        });
    };
    let mut notes = Vec::new();
    let before = map.len();
    map.retain(|key, _| KNOWN_FIELDS.contains(&key.as_str()));
    if map.len() != before {
        notes.push("frontmatter: unsupported fields ignored".into());
    }
    for field in ["name", "description"] {
        if !map.contains_key(field) {
            return Err(Rejected {
                field: known(field),
                reason: "is required",
            });
        }
    }
    for (key, value) in &map {
        let valid = match key.as_str() {
            "name" | "description" | "model" | "isolation" => value.is_string(),
            "tools" | "disallowedTools" => value.is_string() || value.is_array(),
            "permissionMode" => serde_json::from_value::<PermissionMode>(value.clone()).is_ok(),
            "maxTurns" => serde_json::from_value::<u8>(value.clone()).is_ok(),
            _ => value.is_boolean(),
        };
        if !valid {
            return Err(Rejected {
                field: known(key),
                reason: "has an unsupported value",
            });
        }
    }
    let front: Frontmatter =
        serde_json::from_value(serde_json::Value::Object(map)).map_err(|_| Rejected {
            field: "frontmatter",
            reason: "invalid",
        })?;
    let definition = AgentDefinition {
        version: 1,
        name: front.name,
        description: front.description,
        tools: front
            .tools
            .map(|value| tool_list(value, &mut notes))
            .transpose()?,
        disallowed_tools: front
            .disallowed_tools
            .map(|value| tool_list(value, &mut notes))
            .transpose()?
            .unwrap_or_default(),
        model: front.model.filter(|value| value != "inherit"),
        permission_mode: front.permission_mode,
        max_turns: front.max_turns.unwrap_or(crate::session::MAX_CHILD_TURNS),
        isolation: front.isolation,
        background: front.background.unwrap_or(false),
        body: body.to_owned(),
    };
    if !definition.valid() {
        return Err(Rejected {
            field: "definition",
            reason: "over a limit or invalid",
        });
    }
    Ok((definition, notes))
}

pub(super) fn parse(raw: &[u8]) -> Result<String, ToolError> {
    let parsed = match strict(raw) {
        Ok((definition, notes)) => ParsedFile::Definition { definition, notes },
        Err(rejected) => ParsedFile::Skipped {
            diagnostic: format!("{}: {}", rejected.field, rejected.reason),
        },
    };
    serde_json::to_string(&parsed).map_err(|_| ToolError::Configuration)
}

pub(crate) struct Registry {
    pub(crate) definitions: BTreeMap<String, AgentDefinition>,
    pub(crate) diagnostics: Vec<String>,
}

fn diagnostic(file: &str, text: &str) -> String {
    let safe = text.len() <= 120 && text.bytes().all(|byte| (b' '..=b'~').contains(&byte));
    format!("{file}: {}", if safe { text } else { "invalid diagnostic" })
}

impl AgentDefinition {
    pub(crate) fn catalog_entry(&self) -> serde_json::Value {
        serde_json::json!({"name": self.name, "description": self.description, "supported": !self.editor(), "unsupported_reason": self.editor().then_some("editor_execution_unavailable")})
    }
}

pub(crate) fn registry<'a>(
    pin: &DefinitionRegistryPin,
    mut outputs: impl Iterator<Item = &'a str>,
    child_model: &str,
) -> Result<Registry, ToolError> {
    let mut definitions = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for file in &pin.files {
        let output = outputs.next().ok_or(ToolError::Configuration)?;
        bytes += output.len();
        if bytes > MAX_RAW_BYTES {
            return Err(ToolError::Limit);
        }
        match serde_json::from_str(output).map_err(|_| ToolError::Configuration)? {
            ParsedFile::Skipped { diagnostic: text } => {
                diagnostics.push(diagnostic(&file.filename, &text));
            }
            ParsedFile::Definition {
                mut definition,
                notes,
            } => {
                if !definition.valid() {
                    return Err(ToolError::Configuration);
                }
                diagnostics.extend(notes.iter().map(|note| diagnostic(&file.filename, note)));
                if definition
                    .model
                    .as_deref()
                    .is_some_and(|m| m != child_model)
                {
                    definition.model = None;
                    diagnostics.push(diagnostic(
                        &file.filename,
                        "model: not the admitted child model; treated as inherit",
                    ));
                }
                if !seen.insert((file.private, definition.name.clone())) {
                    diagnostics.push(diagnostic(&file.filename, "name: duplicate; skipped"));
                    continue;
                }
                definitions.insert(definition.name.clone(), definition);
            }
        }
    }
    if outputs.next().is_some() {
        return Err(ToolError::Configuration);
    }
    let mut catalog_bytes: usize = definitions
        .values()
        .map(|definition| definition.catalog_entry().to_string().len() + 1)
        .sum();
    while catalog_bytes > MAX_AGENT_CATALOG_BYTES {
        let Some((name, definition)) = definitions.pop_last() else {
            break;
        };
        catalog_bytes -= definition.catalog_entry().to_string().len() + 1;
        diagnostics.push(diagnostic(&name, "dropped by the catalog limit"));
    }
    if diagnostics.len() > MAX_DIAGNOSTICS {
        diagnostics.truncate(MAX_DIAGNOSTICS - 1);
        diagnostics.push("further diagnostics omitted".into());
    }
    Ok(Registry {
        definitions,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn document(fields: &str, body: &str) -> Vec<u8> {
        format!("---\nname: reader\ndescription: Inspect the assignment\n{fields}---\n{body}")
            .into_bytes()
    }

    fn definition(fields: &str) -> AgentDefinition {
        strict(&document(fields, "Guidance only")).unwrap().0
    }

    #[test]
    fn agent_definition_bash_only_narrows_editor_command_authority() {
        let call = ToolCall::Command {
            program: "check".into(),
            args: Vec::new(),
            cwd: ".".into(),
        };
        for name in ["Bash", "command"] {
            let (researcher, notes) =
                strict(&document(&format!("tools: [{name}]\n"), "Body")).unwrap();
            assert!(notes.is_empty());
            assert!(!researcher.allows_worker(&call));
            assert!(!researcher.allows(&call));
            let (editor, _) =
                strict(&document(&format!("tools: [Write, {name}]\n"), "Body")).unwrap();
            assert!(editor.allows_worker(&call));
            let (denied, _) = strict(&document(
                &format!("tools: [Write, {name}]\ndisallowedTools: [Bash]\n"),
                "Body",
            ))
            .unwrap();
            assert!(!denied.allows_worker(&call));
        }
    }

    #[test]
    fn agent_definition_parser_accepts_peer_subset_and_rejects_executable_yaml() {
        for fields in [
            "",
            "tools: Read, Glob, Grep\n",
            "tools: [Read, Skill]\nmodel: inherit\npermissionMode: plan\nmaxTurns: 1\n",
            "description: useful\n",
        ] {
            let result = strict(&document(fields, "Body"));
            assert_eq!(
                result.is_ok(),
                !fields.starts_with("description:"),
                "duplicate fields reject"
            );
        }
        let crlf = b"---\r\nname: reader\r\ndescription: >-\r\n  Inspect carefully\r\ntools: 'Read, Glob'\r\n---\r\nBody";
        assert!(strict(crlf).is_ok());
        for fields in [
            "name: shadow\n",
            "permissionMode: bypassPermissions\n",
            "permissionMode: manual\n",
            "model: null\n",
            "tools: null\n",
            "maxTurns: 0\n",
            "maxTurns: 18\n",
            "isolation: other\n",
            "background: yes\n",
            "tools: &anchor [Read]\n",
            "tools: *anchor\n",
            "tools: !!seq [Read]\n",
            "tools: !execute [Read]\n",
            "tools: [Read]\ntools: [Write]\n",
            "tools: {nested: [Read]}\n",
        ] {
            assert!(
                strict(&document(fields, "Body")).is_err(),
                "unsafe definition rejected"
            );
        }
        for (fields, tools) in [
            ("hooks: {}\n", None),
            ("mcpServers: {}\n", None),
            ("color: red\n", None),
            ("tools: [Git, 'Read*']\n", Some(Vec::new())),
        ] {
            let (definition, notes) = strict(&document(fields, "Body")).unwrap();
            assert_eq!(notes.len(), 1, "{fields}");
            assert_eq!(definition.tools, tools, "{fields}");
        }
        for (fields, field) in [
            ("tools: [Read]\ntools: [Write]\n", "frontmatter"),
            ("maxTurns: 300\n", "maxTurns"),
            ("permissionMode: bypassPermissions\n", "permissionMode"),
            ("model: null\n", "model"),
        ] {
            let rejected = strict(&document(fields, "Body")).unwrap_err();
            assert_eq!(rejected.field, field, "{fields}");
            let skipped = parse(&document(fields, "Body")).unwrap();
            assert!(skipped.contains("Skipped"), "{skipped}");
        }
        for fields in [
            "tools: Bob 's\ntools: &a [Read]\n",
            "color: Bob 's\ntools: &a [Read]\n",
            "tools: *a\n",
        ] {
            assert!(strict(&document(fields, "Body")).is_err(), "{fields}");
        }
        let crafted = b"---\nname: reader\ndescription: Bob 's\ntools: &a [Read]\n---\nBody";
        assert!(strict(crafted).is_err());
        for description in [
            "'literal &anchor *alias !tag'",
            "\"quoted !token\"",
            "|\n  Literal *alias &anchor !!tag",
        ] {
            assert!(
                strict(
                    format!("---\nname: reader\ndescription: {description}\n---\nBody").as_bytes()
                )
                .is_ok()
            );
        }
        assert!(strict(b"---\nname: reader\ndescription: x\n---\n---\nbody").is_ok());
    }

    #[test]
    fn agent_definition_parser_enforces_exact_bounds() {
        let valid = format!(
            "---\nname: {}\ndescription: {}\n---\n{}",
            "a".repeat(64),
            "d".repeat(2048),
            "b".repeat(32 * 1024)
        );
        assert!(strict(valid.as_bytes()).is_ok());
        for (name, description, body) in [
            (65, 2048, 32 * 1024),
            (64, 2049, 32 * 1024),
            (64, 2048, 32 * 1024 + 1),
        ] {
            let raw = format!(
                "---\nname: {}\ndescription: {}\n---\n{}",
                "a".repeat(name),
                "d".repeat(description),
                "b".repeat(body)
            );
            assert!(strict(raw.as_bytes()).is_err());
        }
        let mut raw = document("", "");
        let boundary = raw.len() - 4;
        let padding = 64 * 1024 - raw.len();
        raw.splice(
            boundary..boundary,
            std::iter::repeat_n(b' ', padding - 1).chain(std::iter::once(b'\n')),
        );
        assert_eq!(raw.len(), 64 * 1024);
        assert!(strict(&raw).is_ok());
        raw.insert(boundary, b' ');
        assert!(strict(&raw).is_err());
        assert!(definition(&format!("tools: [{}]\n", vec!["Read"; 32].join(","))).valid());
        assert!(
            strict(&document(
                &format!("tools: [{}]\n", vec!["Read"; 33].join(",")),
                ""
            ))
            .is_err()
        );
        for (count, expected) in [(4089, true), (4090, false)] {
            let raw = format!(
                "---\nname: reader\ndescription: x\nx: [{}]\n---\n",
                vec!["x"; count].join(",")
            );
            assert_eq!(
                super::super::skills::definition_document(raw.as_bytes()).is_ok(),
                expected
            );
        }
        for (depth, expected) in [(31, true), (32, false)] {
            let raw = format!(
                "---\nname: reader\ndescription: x\nx: {}x{}\n---\n",
                "[".repeat(depth),
                "]".repeat(depth)
            );
            assert_eq!(
                super::super::skills::definition_document(raw.as_bytes()).is_ok(),
                expected
            );
        }
    }

    fn output(definition: &AgentDefinition) -> String {
        serde_json::to_string(&ParsedFile::Definition {
            definition: definition.clone(),
            notes: Vec::new(),
        })
        .unwrap()
    }

    fn pin(names: &[(bool, String)]) -> DefinitionRegistryPin {
        let files: Vec<_> = names
            .iter()
            .map(|(private, filename)| DefinitionFilePin {
                private: *private,
                filename: filename.clone(),
                digest: [1; 32],
            })
            .collect();
        DefinitionRegistryPin {
            digest: digest(&serde_json::to_vec(&files).unwrap()),
            files,
        }
    }

    #[test]
    fn agent_definition_registry_skips_invalid_files_and_private_wins() {
        let project = parse(&document("tools: Read\n", "Project")).unwrap();
        let private = parse(&document("tools: Glob\n", "Private")).unwrap();
        let invalid = parse(&document("maxTurns: 300\n", "Invalid")).unwrap();
        let aliased = parse(&document("model: sonnet\n", "Aliased")).unwrap();
        let pinned = pin(&[(false, "a.md".into()), (true, "b.md".into())]);
        assert!(pinned.valid());
        let registry = registry(
            &pinned,
            [project.as_str(), private.as_str()].into_iter(),
            "m",
        )
        .unwrap();
        assert_eq!(registry.definitions["reader"].body, "Private");
        assert!(super::registry(&pinned, [project.as_str()].into_iter(), "m").is_err());
        let both_public = pin(&[(false, "a.md".into()), (false, "b.md".into())]);
        let collided = super::registry(
            &both_public,
            [project.as_str(), private.as_str()].into_iter(),
            "m",
        )
        .unwrap();
        assert_eq!(collided.diagnostics, ["b.md: name: duplicate; skipped"]);
        let skipped = super::registry(
            &both_public,
            [invalid.as_str(), aliased.as_str()].into_iter(),
            "m",
        )
        .unwrap();
        assert_eq!(
            skipped.diagnostics,
            [
                "a.md: maxTurns: has an unsupported value",
                "b.md: model: not the admitted child model; treated as inherit"
            ]
        );
        assert!(skipped.definitions["reader"].model.is_none());
        let exact = parse(&document("model: m\n", "Exact")).unwrap();
        let kept = super::registry(
            &both_public,
            [exact.as_str(), exact.as_str()].into_iter(),
            "m",
        )
        .unwrap();
        assert_eq!(kept.definitions["reader"].model.as_deref(), Some("m"));
        let hostile = r#"{"Skipped":{"diagnostic":"bad\u001b[31m"}}"#;
        let escaped = super::registry(&both_public, [hostile, hostile].into_iter(), "m").unwrap();
        assert_eq!(escaped.diagnostics[0], "a.md: invalid diagnostic");
    }

    #[test]
    fn agent_definition_registry_drops_excess_definitions_at_the_catalog_limit() {
        let count = 64;
        let outputs: Vec<_> = (0..count)
            .map(|index| {
                let mut definition = definition("tools: Read\n");
                definition.name = format!("reader{index:03}");
                definition.description = "d".repeat(200);
                output(&definition)
            })
            .collect();
        let names: Vec<_> = (0..count)
            .map(|index| (false, format!("reader{index:03}.md")))
            .collect();
        let registry =
            super::registry(&pin(&names), outputs.iter().map(String::as_str), "m").unwrap();
        let kept = registry.definitions.len();
        assert!(kept < count);
        assert_eq!(registry.diagnostics.len(), count - kept);
        assert!(registry.diagnostics[0].ends_with("dropped by the catalog limit"));
        assert!(registry.definitions.contains_key("reader000"));
        assert!(
            registry
                .definitions
                .values()
                .map(|definition| definition.catalog_entry().to_string().len() + 1)
                .sum::<usize>()
                <= MAX_AGENT_CATALOG_BYTES
        );
    }

    #[test]
    fn agent_definition_registry_bounds_aggregate_output_bytes() {
        let mut definitions: Vec<_> = (0..32)
            .map(|index| {
                let mut definition = definition("tools: Read\n");
                definition.name = format!("reader{index:03}");
                definition.body.clear();
                definition
            })
            .collect();
        let overhead: usize = definitions
            .iter()
            .map(|definition| output(definition).len())
            .sum();
        let mut remaining = MAX_RAW_BYTES - overhead;
        for definition in &mut definitions {
            let bytes = remaining.min(32 * 1024);
            definition.body = "b".repeat(bytes);
            remaining -= bytes;
        }
        assert_eq!(remaining, 0);
        let names: Vec<_> = definitions
            .iter()
            .map(|definition| (false, format!("{}.md", definition.name)))
            .collect();
        let pin = pin(&names);
        let boundary: Vec<_> = definitions.iter().map(output).collect();
        assert_eq!(
            boundary.iter().map(String::len).sum::<usize>(),
            MAX_RAW_BYTES
        );
        assert!(super::registry(&pin, boundary.iter().map(String::as_str), "m").is_ok());
        definitions.last_mut().unwrap().body.push('b');
        let overflow: Vec<_> = definitions.iter().map(output).collect();
        assert!(super::registry(&pin, overflow.iter().map(String::as_str), "m").is_err());
    }

    #[test]
    fn agent_definition_snapshots_reject_unsafe_entries_and_pin_bytes_once() {
        let temp = tempfile::tempdir().unwrap();
        let directory = fs::open_directory(temp.path()).unwrap();
        let original = document("", "Original");
        std::fs::write(temp.path().join("reader.md"), &original).unwrap();
        let mut snapshots = DefinitionSnapshots::default();
        scan(&directory, false, &mut snapshots, &mut 0).unwrap();
        std::fs::rename(temp.path().join("reader.md"), temp.path().join("old.md")).unwrap();
        std::fs::write(temp.path().join("reader.md"), document("", "Replacement")).unwrap();
        let call = ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: digest(&original),
        };
        let encoded = snapshots.payload(&call).unwrap().unwrap();
        let pinned = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(pinned, original, "no reopen after snapshot");
        assert!(
            snapshots
                .payload(&ToolCall::ParseAgentDefinition {
                    index: 0,
                    raw_digest: [0; 32]
                })
                .is_err()
        );
        std::fs::remove_file(temp.path().join("old.md")).unwrap();
        std::fs::remove_file(temp.path().join("reader.md")).unwrap();
        for case in [
            "symlink",
            "hardlink",
            "fifo",
            "set-id",
            "protected",
            "directory",
            "public-private",
        ] {
            let path = temp.path().join(if case == "protected" {
                ".env.md"
            } else {
                "reader.md"
            });
            match case {
                "symlink" => std::os::unix::fs::symlink("missing", &path).unwrap(),
                "hardlink" => {
                    std::fs::write(temp.path().join("target"), &original).unwrap();
                    std::fs::hard_link(temp.path().join("target"), &path).unwrap();
                }
                "fifo" => nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => {
                    std::fs::write(&path, &original).unwrap();
                    std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(if case == "set-id" {
                            0o4600
                        } else {
                            0o644
                        }),
                    )
                    .unwrap();
                }
            }
            assert!(
                scan(
                    &directory,
                    case == "public-private",
                    &mut DefinitionSnapshots::default(),
                    &mut 0
                )
                .is_err(),
                "unsafe entry rejected: {case}"
            );
            if case == "directory" {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            if case == "hardlink" {
                std::fs::remove_file(temp.path().join("target")).unwrap();
            }
        }
    }

    #[test]
    fn agent_definition_discovery_enforces_file_scan_and_aggregate_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let directory = fs::open_directory(temp.path()).unwrap();
        for index in 0..256 {
            std::fs::write(temp.path().join(format!("file{index}.txt")), b"").unwrap();
        }
        scan(
            &directory,
            false,
            &mut DefinitionSnapshots::default(),
            &mut 0,
        )
        .unwrap();
        std::fs::write(temp.path().join("overflow.txt"), b"").unwrap();
        assert!(matches!(
            scan(
                &directory,
                false,
                &mut DefinitionSnapshots::default(),
                &mut 0
            ),
            Err(ToolError::Limit)
        ));
        for entry in std::fs::read_dir(temp.path()).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for index in 0..64 {
            std::fs::write(temp.path().join(format!("reader{index}.md")), b"").unwrap();
        }
        scan(
            &directory,
            false,
            &mut DefinitionSnapshots::default(),
            &mut 0,
        )
        .unwrap();
        std::fs::write(temp.path().join("overflow.md"), b"").unwrap();
        assert!(matches!(
            scan(
                &directory,
                false,
                &mut DefinitionSnapshots::default(),
                &mut 0
            ),
            Err(ToolError::Limit)
        ));
        for entry in std::fs::read_dir(temp.path()).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for index in 0..16 {
            std::fs::write(
                temp.path().join(format!("reader{index}.md")),
                vec![b' '; 64 * 1024],
            )
            .unwrap();
        }
        let mut bytes = 0;
        scan(
            &directory,
            false,
            &mut DefinitionSnapshots::default(),
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, MAX_RAW_BYTES);
        std::fs::write(temp.path().join("overflow.md"), b"x").unwrap();
        assert!(matches!(
            scan(
                &directory,
                false,
                &mut DefinitionSnapshots::default(),
                &mut 0
            ),
            Err(ToolError::Limit)
        ));
    }

    #[test]
    fn agent_definition_modes_and_tools_only_narrow_research_authority() {
        let read = ToolCall::Read {
            path: "file".into(),
            offset: 0,
            limit: 1,
        };
        let write = ToolCall::Write {
            path: "file".into(),
            expected_digest: None,
            content: "data".into(),
        };
        for mode in ["default", "acceptEdits", "plan", "auto", "dontAsk"] {
            let definition = definition(&format!("permissionMode: {mode}\n"));
            assert!(definition.allows(&read));
            assert!(!definition.allows(&write));
        }
        assert!(!definition("tools: Glob\n").allows(&read));
        assert!(!definition("disallowedTools: Read\n").allows(&read));
        assert!(definition("tools: Read, Edit\n").editor());
        assert!(definition("isolation: worktree\n").editor());
        assert!(definition("background: true\n").editor());
        assert!(
            !super::super::catalog::call_schemas()
                .iter()
                .any(|schema| schema["properties"]["operation"]["enum"][0]
                    == "parse_agent_definition")
        );
    }
}

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
        if self.allows(call) {
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
            && (1..=17).contains(&self.max_turns)
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
        "list" | "read" | "search" | "edit" | "write" | "mkdir" | "skill" | "mcp_list" => {
            vec![name]
        }
        _ => return None,
    })
}

fn tool_list(value: serde_json::Value) -> Result<Vec<String>, ToolError> {
    let names = match value {
        serde_json::Value::String(value) => value
            .split(',')
            .map(|name| name.trim().to_owned())
            .collect(),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(ToolError::Configuration)
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(ToolError::Configuration),
    };
    if names.len() > 32 {
        return Err(ToolError::Limit);
    }
    let mut result = BTreeSet::new();
    for name in names {
        if name.len() > 128 {
            return Err(ToolError::Limit);
        }
        for operation in supported_tool(&name).ok_or(ToolError::Configuration)? {
            result.insert(operation.to_owned());
        }
    }
    Ok(result.into_iter().collect())
}

pub(super) fn parse(raw: &[u8]) -> Result<String, ToolError> {
    let (value, body) = super::skills::definition_document(raw)?;
    if value
        .as_object()
        .is_none_or(|map| map.values().any(serde_json::Value::is_null))
    {
        return Err(ToolError::Configuration);
    }
    let front: Frontmatter = serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
    let definition = AgentDefinition {
        version: 1,
        name: front.name,
        description: front.description,
        tools: front.tools.map(tool_list).transpose()?,
        disallowed_tools: front
            .disallowed_tools
            .map(tool_list)
            .transpose()?
            .unwrap_or_default(),
        model: front.model.filter(|value| value != "inherit"),
        permission_mode: front.permission_mode,
        max_turns: front.max_turns.unwrap_or(17),
        isolation: front.isolation,
        background: front.background.unwrap_or(false),
        body: body.to_owned(),
    };
    if !definition.valid() {
        return Err(ToolError::Configuration);
    }
    serde_json::to_string(&definition).map_err(|_| ToolError::Configuration)
}

pub(crate) fn registry<'a>(
    pin: &DefinitionRegistryPin,
    mut outputs: impl Iterator<Item = &'a str>,
) -> Result<BTreeMap<String, AgentDefinition>, ToolError> {
    let mut registry = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut count = 0;
    let mut bytes = 0;
    for file in &pin.files {
        let output = outputs.next().ok_or(ToolError::Configuration)?;
        count += 1;
        bytes += output.len();
        if bytes > MAX_RAW_BYTES {
            return Err(ToolError::Limit);
        }
        let definition: AgentDefinition =
            serde_json::from_str(output).map_err(|_| ToolError::Configuration)?;
        if !definition.valid() || !seen.insert((file.private, definition.name.clone())) {
            return Err(ToolError::Configuration);
        }
        registry.insert(definition.name.clone(), definition);
        if registry.len() > MAX_DEFINITIONS {
            return Err(ToolError::Limit);
        }
    }
    if count != pin.files.len() || outputs.next().is_some() {
        return Err(ToolError::Configuration);
    }
    Ok(registry)
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
        serde_json::from_str(&parse(&document(fields, "Guidance only")).unwrap()).unwrap()
    }

    #[test]
    fn agent_definition_parser_accepts_peer_subset_and_rejects_executable_yaml() {
        for fields in [
            "",
            "tools: Read, Glob, Grep\n",
            "tools: [Read, Skill]\nmodel: inherit\npermissionMode: plan\nmaxTurns: 1\n",
            "description: useful\n",
        ] {
            let result = parse(&document(fields, "Body"));
            assert_eq!(
                result.is_ok(),
                !fields.starts_with("description:"),
                "duplicate fields reject"
            );
        }
        let crlf = b"---\r\nname: reader\r\ndescription: >-\r\n  Inspect carefully\r\ntools: 'Read, Glob'\r\n---\r\nBody";
        assert!(parse(crlf).is_ok());
        for fields in [
            "name: shadow\n",
            "hooks: {}\n",
            "mcpServers: {}\n",
            "permissionMode: bypassPermissions\n",
            "permissionMode: manual\n",
            "model: null\n",
            "tools: null\n",
            "maxTurns: 0\n",
            "maxTurns: 18\n",
            "tools: [Bash]\n",
            "tools: [Agent]\n",
            "tools: [Git]\n",
            "tools: [mcp_call]\n",
            "tools: ['Read*']\n",
            "isolation: other\n",
            "background: yes\n",
            "tools: &anchor [Read]\n",
            "tools: *anchor\n",
            "tools: !!seq [Read]\n",
            "tools: !execute [Read]\n",
            "<<: {tools: Read}\n",
            "tools: [Read]\ntools: [Write]\n",
            "tools: {nested: [Read]}\n",
        ] {
            assert!(
                parse(&document(fields, "Body")).is_err(),
                "unsafe definition rejected"
            );
        }
        for description in [
            "'literal &anchor *alias !tag'",
            "\"quoted !token\"",
            "|\n  Literal *alias &anchor !!tag",
        ] {
            assert!(
                parse(
                    format!("---\nname: reader\ndescription: {description}\n---\nBody").as_bytes()
                )
                .is_ok()
            );
        }
        assert!(parse(b"---\nname: reader\ndescription: x\n---\n---\nbody").is_ok());
    }

    #[test]
    fn agent_definition_parser_enforces_exact_bounds() {
        let valid = format!(
            "---\nname: {}\ndescription: {}\n---\n{}",
            "a".repeat(64),
            "d".repeat(2048),
            "b".repeat(32 * 1024)
        );
        assert!(parse(valid.as_bytes()).is_ok());
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
            assert!(parse(raw.as_bytes()).is_err());
        }
        let mut raw = document("", "");
        let boundary = raw.len() - 4;
        let padding = 64 * 1024 - raw.len();
        raw.splice(
            boundary..boundary,
            std::iter::repeat_n(b' ', padding - 1).chain(std::iter::once(b'\n')),
        );
        assert_eq!(raw.len(), 64 * 1024);
        assert!(parse(&raw).is_ok());
        raw.insert(boundary, b' ');
        assert!(matches!(parse(&raw), Err(ToolError::Limit)));
        assert!(definition(&format!("tools: [{}]\n", vec!["Read"; 32].join(","))).valid());
        assert!(
            parse(&document(
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

    #[test]
    fn agent_definition_registry_is_atomic_private_wins_and_collisions_refuse() {
        let project = parse(&document("tools: Read\n", "Project")).unwrap();
        let private = parse(&document("tools: Glob\n", "Private")).unwrap();
        let files = vec![
            DefinitionFilePin {
                private: false,
                filename: "a.md".into(),
                digest: [1; 32],
            },
            DefinitionFilePin {
                private: true,
                filename: "b.md".into(),
                digest: [2; 32],
            },
        ];
        let mut pin = DefinitionRegistryPin {
            digest: digest(&serde_json::to_vec(&files).unwrap()),
            files,
        };
        assert!(pin.valid());
        let registry = registry(&pin, [project.as_str(), private.as_str()].into_iter()).unwrap();
        assert_eq!(registry["reader"].body, "Private");
        assert!(super::registry(&pin, [project.as_str()].into_iter()).is_err());
        pin.files[1].private = false;
        assert!(super::registry(&pin, [project.as_str(), private.as_str()].into_iter()).is_err());
        pin.digest = [0; 32];
        assert!(!pin.valid());
        for count in [64, 65] {
            let outputs: Vec<_> = (0..count)
                .map(|index| {
                    let mut definition = definition("tools: Read\n");
                    definition.name = format!("reader{index:03}");
                    serde_json::to_string(&definition).unwrap()
                })
                .collect();
            let files: Vec<_> = (0..count)
                .map(|index| DefinitionFilePin {
                    private: false,
                    filename: format!("reader{index:03}.md"),
                    digest: [1; 32],
                })
                .collect();
            let pin = DefinitionRegistryPin {
                digest: digest(&serde_json::to_vec(&files).unwrap()),
                files,
            };
            assert_eq!(
                super::registry(&pin, outputs.iter().map(String::as_str)).is_ok(),
                count == 64
            );
        }
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
            .map(|definition| serde_json::to_string(definition).unwrap().len())
            .sum();
        let mut remaining = MAX_RAW_BYTES - overhead;
        for definition in &mut definitions {
            let bytes = remaining.min(32 * 1024);
            definition.body = "b".repeat(bytes);
            remaining -= bytes;
        }
        assert_eq!(remaining, 0);
        let files: Vec<_> = definitions
            .iter()
            .map(|definition| DefinitionFilePin {
                private: false,
                filename: format!("{}.md", definition.name),
                digest: [1; 32],
            })
            .collect();
        let pin = DefinitionRegistryPin {
            digest: digest(&serde_json::to_vec(&files).unwrap()),
            files,
        };
        let output = |definitions: &[AgentDefinition]| {
            definitions
                .iter()
                .map(|definition| serde_json::to_string(definition).unwrap())
                .collect::<Vec<_>>()
        };
        let boundary = output(&definitions);
        assert_eq!(
            boundary.iter().map(String::len).sum::<usize>(),
            MAX_RAW_BYTES
        );
        assert!(super::registry(&pin, boundary.iter().map(String::as_str)).is_ok());
        definitions.last_mut().unwrap().body.push('b');
        let overflow = output(&definitions);
        assert!(super::registry(&pin, overflow.iter().map(String::as_str)).is_err());
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
                "fifo" => rustix::fs::mknodat(
                    &directory,
                    "reader.md",
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::RUSR,
                    0,
                )
                .unwrap(),
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

use super::ToolError;
use super::fs::{open_absolute, read_regular};
use super::toolchains::{RuntimeGrant, validate_selection};
use super::types::*;
use crate::store::{RecordFormat, RecordKind, StateRoot};
use crate::tools::process::MAX_ARGUMENT_BYTES;
use crate::validate::{digest, hex_digest, valid_digest, valid_name, valid_relative};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_WORKSPACE_PATHS: usize = 32;
pub(super) const MAX_MCP_SERVERS: usize = 4;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub workspace_paths: Vec<String>,
    pub write: bool,
    #[serde(default, skip_serializing_if = "UnguardedGrant::is_denied")]
    pub child_unguarded: UnguardedGrant,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub child_web: bool,
    pub commands: Vec<Program>,
    pub skills: Vec<Skill>,
    pub mcp: Vec<Server>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_grants: Vec<RuntimeGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<Program>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locale: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<super::network::Network>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearch>,
}

// The backend speaks the Brave Search API; `key` names an ARANY_WEB_SEARCH_* variable and is never stored here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebSearch {
    pub endpoint: String,
    pub key: String,
}

impl WebSearch {
    const KEY_PREFIX: &'static str = "ARANY_WEB_SEARCH_";
    const MAX_KEY_BYTES: usize = 64;

    #[cfg(target_os = "linux")]
    pub(crate) fn host(&self) -> Option<String> {
        let call = ToolCall::WebFetch {
            url: self.endpoint.clone(),
        };
        super::web::host(&call)
    }

    pub(super) fn valid(&self) -> bool {
        let call = ToolCall::WebFetch {
            url: self.endpoint.clone(),
        };
        call.valid()
            && self.key.strip_prefix(Self::KEY_PREFIX).is_some_and(|name| {
                !name.is_empty()
                    && self.key.len() <= Self::MAX_KEY_BYTES
                    && name.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                    })
            })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Program {
    pub name: String,
    pub executable: String,
    pub sha256: String,
    pub interpreter: bool,
    pub inputs: Vec<ProgramInput>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramInput {
    pub path: String,
    pub destination: String,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Skill {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_model_invocation: bool,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Server {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<Program>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuth>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpAuth {
    Oauth,
}

impl Skill {
    fn valid_entry(&self) -> bool {
        valid_name(&self.name)
            && !self.description.trim().is_empty()
            && self.description.len() <= 1024
            && valid_absolute(&self.directory)
            && self.files.contains_key("SKILL.md")
            && self.files.len() <= 32
            && self
                .files
                .iter()
                .all(|(path, hash)| valid_relative(path, false) && valid_digest(hash))
    }
}

impl Server {
    fn valid_entry(&self) -> bool {
        valid_name(&self.name)
            && !self.tools.is_empty()
            && self.tools.len() <= 64
            && self.tools.iter().collect::<BTreeSet<_>>().len() == self.tools.len()
            && self.tools.iter().all(|tool| {
                !tool.is_empty() && tool.len() <= 128 && !tool.chars().any(char::is_control)
            })
            && self.valid_transport()
            && self.args.len() <= 64
            && self
                .args
                .iter()
                .all(|arg| arg.len() <= MAX_ARGUMENT_BYTES && !arg.contains('\0'))
    }

    // Local servers run a pinned program; remote ones speak streamable HTTP through the egress proxy, which is Linux-only.
    fn valid_transport(&self) -> bool {
        match (&self.program, &self.url) {
            (Some(_), None) => self.auth.is_none(),
            (None, Some(url)) => {
                cfg!(target_os = "linux") && self.args.is_empty() && super::web::valid_endpoint(url)
            }
            _ => false,
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn host(&self) -> Option<String> {
        super::web::endpoint_host(self.url.as_deref()?)
    }

    pub(super) fn catalog_row(&self, protocol: &str) -> Value {
        let remote = self.url.is_some();
        let protocol = if remote {
            super::catalog::REMOTE_MCP_PROTOCOL
        } else {
            protocol
        };
        let mut row = serde_json::json!({"name":self.name,"tools":self.tools,"protocol":protocol});
        if remote {
            row["transport"] = "https".into();
        }
        row
    }
}

pub(crate) const TOOLS_CONFIG: RecordFormat = RecordFormat {
    kind: RecordKind::ToolsConfig,
    accepts: |bytes| Config::parse(bytes).is_ok(),
};

impl Config {
    pub(crate) fn bind_environment(
        &mut self,
        environment: &super::CommandEnvironment,
        protected: &[&std::path::Path],
    ) -> Result<(), ToolError> {
        self.locale = environment.locale();
        self.environment_sha256 = Some(environment.sha256().to_owned());
        let path = super::toolchains::path_grants(
            &environment.path_string(),
            &self.runtime_grants,
            protected,
        )?;
        self.runtime_grants.extend(path);
        let targets = super::toolchains::link_target_grants(&self.runtime_grants, protected);
        self.runtime_grants.extend(targets);
        let reads = super::GuardSettings::load()?.grants(protected);
        self.runtime_grants.extend(reads);
        Ok(())
    }

    pub(crate) fn load(state: &StateRoot) -> Result<Self, ToolError> {
        let bytes = state
            .record(TOOLS_CONFIG.kind)
            .read()
            .map_err(|_| ToolError::Configuration)?
            .ok_or(ToolError::MissingConfiguration)?;
        Self::parse(&bytes)
    }

    pub(super) fn parse(bytes: &[u8]) -> Result<Self, ToolError> {
        let value =
            super::skills::parse_json(bytes, 64 * 1024).map_err(|_| ToolError::Configuration)?;
        let config: Self = serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
        config.validate()?;
        if !config.runtime_grants.is_empty()
            || !config.locale.is_empty()
            || config.environment_sha256.is_some()
            || config.network.as_ref().is_some_and(|network| {
                !network.deny.is_empty()
                    || network.allow.len() > super::network::MAX_NETWORK_PATTERNS
            })
        {
            return Err(ToolError::Configuration);
        }
        Ok(config)
    }

    pub(crate) fn remote_server(&self, call: &ToolCall) -> Option<&Server> {
        let (ToolCall::McpList { server } | ToolCall::McpCall { server, .. }) = call else {
            return None;
        };
        self.mcp
            .iter()
            .find(|candidate| &candidate.name == server && candidate.url.is_some())
    }

    pub(crate) fn validate(&self) -> Result<(), ToolError> {
        if self.child_unguarded != UnguardedGrant::Denied
            || self.workspace_paths.is_empty()
            || self.workspace_paths.len() > MAX_WORKSPACE_PATHS
            || self.workspace_paths.iter().any(|path| {
                !valid_relative(path, false) && !(path == "." && self.workspace_paths.len() == 1)
            })
            || self.commands.len() > 8
            || self.skills.len() > MAX_SKILLS
            || self.mcp.len() > MAX_MCP_SERVERS
        {
            return Err(ToolError::Configuration);
        }
        if self.child_web && cfg!(not(target_os = "linux")) {
            return Err(ToolError::Configuration);
        }
        validate_selection(&self.runtime_grants)?;
        if !super::command_environment::valid_locale(&self.locale)
            || self
                .environment_sha256
                .as_deref()
                .is_some_and(|digest| !valid_digest(digest))
        {
            return Err(ToolError::Configuration);
        }
        if self
            .network
            .as_ref()
            .is_some_and(|network| !network.valid())
        {
            return Err(ToolError::Configuration);
        }
        if self
            .web_search
            .as_ref()
            .is_some_and(|search| cfg!(not(target_os = "linux")) || !search.valid())
        {
            return Err(ToolError::Configuration);
        }
        if !disjoint_roots(&self.workspace_paths) {
            return Err(ToolError::Configuration);
        }
        for program in self
            .commands
            .iter()
            .chain(self.mcp.iter().filter_map(|server| server.program.as_ref()))
        {
            program.validate()?;
        }
        if let Some(git) = &self.git {
            git.validate()?;
            if git.name != "git" || git.interpreter || !git.inputs.is_empty() {
                return Err(ToolError::Configuration);
            }
        }
        if !self.mcp.iter().all(Server::valid_entry) || !self.skills.iter().all(Skill::valid_entry)
        {
            return Err(ToolError::Configuration);
        }
        for names in [
            self.commands
                .iter()
                .chain(self.mcp.iter().filter_map(|server| server.program.as_ref()))
                .map(|item| &item.name)
                .collect::<Vec<_>>(),
            self.skills.iter().map(|item| &item.name).collect(),
            self.mcp.iter().map(|item| &item.name).collect(),
        ] {
            if names.iter().collect::<BTreeSet<_>>().len() != names.len() {
                return Err(ToolError::Configuration);
            }
        }
        Ok(())
    }

    pub(super) fn resource_paths(&self) -> impl Iterator<Item = &str> {
        self.commands
            .iter()
            .chain(self.mcp.iter().filter_map(|server| server.program.as_ref()))
            .chain(self.git.iter())
            .flat_map(|program| {
                std::iter::once(program.executable.as_str())
                    .chain(program.inputs.iter().map(|input| input.path.as_str()))
            })
            .chain(self.skills.iter().map(|skill| skill.directory.as_str()))
    }

    // Git output spans the repository, so typed Git reads require the whole-Workspace grant.
    pub(crate) fn git_available(&self) -> bool {
        self.git.is_some() && self.workspace_paths == ["."]
    }

    pub(super) fn pinned_gh(&self) -> Option<&Program> {
        self.commands.iter().find(|program| {
            program.name == "gh" && !program.interpreter && program.inputs.is_empty()
        })
    }

    pub(super) fn discover_skills(&mut self, workspace: &std::path::Path) -> Result<(), ToolError> {
        if !self.permits_project_skills() {
            return Ok(());
        }
        super::skills::discover(workspace, &mut self.skills)?;
        if serde_json::to_vec(self)
            .map_err(|_| ToolError::Configuration)?
            .len()
            > 64 * 1024
        {
            return Err(ToolError::Limit);
        }
        self.validate()
    }

    pub(super) fn permits_project_skills(&self) -> bool {
        self.workspace_paths
            .iter()
            .any(|path| path == "." || std::path::Path::new(".agents/skills").starts_with(path))
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed config is serializable"))
    }

    pub(super) fn receipt(&self) -> ToolPolicyReceipt {
        ToolPolicyReceipt {
            host_epoch: None,
            command_tools_digest: None,
            host_prefixes: Vec::new(),
            contract_version: 1,
            config_digest: self.digest(),
            permissions_digest: None,
            max_context_bytes: MAX_TOOL_CONTEXT_BYTES as u32,
            unguarded_rules_digest: None,
            unguarded: UnguardedGrant::HumanOnly,
            child_unguarded: UnguardedGrant::Denied,
            child_web: self.child_web && cfg!(target_os = "linux"),
            git: self.git.as_ref().map(|git| GitRunPin {
                executable: git.executable.clone(),
                sha256: git.sha256.clone(),
                hooks: None,
                repository: None,
            }),
            guard: GuardPin {
                mode: super::GuardMode::Auto,
                sandbox: true,
                saved_reads: self
                    .runtime_grants
                    .iter()
                    .filter(|grant| grant.kind == super::toolchains::RuntimeKind::ExtraRead)
                    .map(|grant| ReadPin {
                        path: grant.path.to_string_lossy().into_owned(),
                        device: grant.device,
                        inode: grant.inode,
                    })
                    .collect(),
            },
        }
    }

    pub(crate) fn in_workspace_scope(&self, path: &str) -> bool {
        self.workspace_paths == ["."]
            || self
                .workspace_paths
                .iter()
                .any(|root| std::path::Path::new(path).starts_with(root))
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        if !call.valid() || call.mutates() && !self.write && !call.is_unguarded() {
            return false;
        }
        let path = match call {
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::Rewind { .. },
            } => return false,
            ToolCall::ParseAgentDefinition { .. } => return true,
            ToolCall::List { path }
            | ToolCall::Read { path, .. }
            | ToolCall::Search { path, .. }
            | ToolCall::Glob { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::Delete { path, .. }
            | ToolCall::MultiEdit {
                file_path: path, ..
            } => path,
            ToolCall::Move {
                source,
                destination,
                ..
            } => {
                return self.in_workspace_scope(source) && self.in_workspace_scope(destination);
            }
            ToolCall::CommandUnguarded { .. } => return true,
            ToolCall::Command { program, .. } => {
                return self.commands.iter().any(|value| &value.name == program);
            }
            ToolCall::ApplyChangeset {
                source:
                    crate::session::worker::ChangesetSource::CommandReceipt { .. }
                    | crate::session::worker::ChangesetSource::EditorWorkspace { .. },
            } => {
                return call
                    .changeset_paths()
                    .into_iter()
                    .all(|path| self.in_workspace_scope(path));
            }
            ToolCall::Skill { name, resource } => {
                return self.skills.iter().any(|value| {
                    &value.name == name
                        && value
                            .files
                            .contains_key(resource.as_deref().unwrap_or("SKILL.md"))
                });
            }
            ToolCall::McpList { server } => {
                return self.mcp.iter().any(|value| &value.name == server);
            }
            ToolCall::GitPush { .. } => return self.git_available() && self.write,
            ToolCall::GhPrCreate { .. } => {
                return self.git_available() && self.write && self.pinned_gh().is_some();
            }
            ToolCall::GhPrList { .. } | ToolCall::GhPrView { .. } => {
                return self.git_available() && self.pinned_gh().is_some();
            }
            ToolCall::GitStatus { .. }
            | ToolCall::GitDiff { .. }
            | ToolCall::GitLog { .. }
            | ToolCall::GitCommit { .. }
            | ToolCall::GitBranch { .. }
            | ToolCall::GitSwitch { .. } => return self.git_available(),
            ToolCall::McpCall { server, tool, .. } => {
                return self
                    .mcp
                    .iter()
                    .any(|value| &value.name == server && value.tools.contains(tool));
            }
            ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => {
                return self.admits_web(call);
            }
        };
        if matches!(call, ToolCall::Read { .. }) && super::spill::read_reference(path).is_some() {
            return true;
        }
        (path.is_empty() || path == ".") && !call.mutates() || self.in_workspace_scope(path)
    }
}

fn disjoint_roots(paths: &[String]) -> bool {
    let roots: BTreeSet<_> = paths.iter().collect();
    roots.len() == paths.len()
        && !roots.iter().any(|left| {
            roots
                .iter()
                .any(|right| left != right && std::path::Path::new(left).starts_with(right))
        })
}

pub(super) const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

impl Program {
    fn validate(&self) -> Result<(), ToolError> {
        if !valid_name(&self.name)
            || !valid_absolute(&self.executable)
            || !valid_digest(&self.sha256)
            || self.inputs.len() > 16
            || self.inputs.iter().any(|input| {
                !valid_absolute(&input.path)
                    || !valid_relative(&input.destination, false)
                    || !valid_digest(&input.sha256)
            })
            || self
                .inputs
                .iter()
                .map(|input| &input.destination)
                .collect::<BTreeSet<_>>()
                .len()
                != self.inputs.len()
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }

    pub(super) fn verify_executable_to(
        &self,
        output: &mut impl std::io::Write,
    ) -> Result<(), ToolError> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let mut file = open_absolute(&self.executable)?;
        let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(ToolError::Path);
        }
        if metadata.len() > MAX_EXECUTABLE_BYTES {
            return Err(ToolError::Limit);
        }
        let mut header = [0; 4];
        file.read_exact(&mut header)
            .map_err(|_| ToolError::ChangedInput)?;
        if !super::executable::has_magic(&header, super::executable::EXECUTABLES.program_magics) {
            return Err(ToolError::ChangedInput);
        }
        let mut hash = Sha256::new();
        hash.update(header);
        output
            .write_all(&header)
            .map_err(|_| ToolError::Operation)?;
        let mut total = 4u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(|_| ToolError::Operation)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_EXECUTABLE_BYTES {
                return Err(ToolError::Limit);
            }
            hash.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|_| ToolError::Operation)?;
        }
        if hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            != self.sha256
        {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }

    pub(super) fn verify(&self) -> Result<(), ToolError> {
        self.verify_executable_to(&mut std::io::sink())?;
        for input in &self.inputs {
            if hex_digest(&read_regular(open_absolute(&input.path)?, MAX_FILE_BYTES)?)
                != input.sha256
            {
                return Err(ToolError::ChangedInput);
            }
        }
        Ok(())
    }
}

pub(crate) fn valid_absolute(value: &str) -> bool {
    value
        .strip_prefix('/')
        .is_some_and(|value| valid_relative(value, false))
}

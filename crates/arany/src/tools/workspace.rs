use super::approval::ApprovalMode;
use super::config::{Config, Program};
use super::toolchains::{
    Candidate, MAX_RUNTIME_GRANTS, RuntimeGrant, default_profile_selection, discover,
    profile_order, validate_selection,
};
use super::types::MAX_SNAPSHOT_BYTES;
use super::{ToolError, fs};
use crate::session::WorkspaceIdentity;
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use crate::validate::{MAX_PATH_BYTES, hex_digest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn mention_paths(objective: &str) -> Result<Vec<PathBuf>, ToolError> {
    if objective.len() > crate::session::MAX_USER_MESSAGE_BYTES {
        return Err(ToolError::Limit);
    }
    let mut paths = Vec::new();
    let mut remaining = objective;
    let mut initial = true;
    while let Some(index) = remaining.find('@') {
        let boundary = index == 0 && initial
            || remaining[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let tail = &remaining[index + 1..];
        initial = false;
        if !boundary || tail.starts_with('@') {
            remaining = tail;
            continue;
        }
        let (path, used) = if tail.starts_with('"') {
            let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<String>();
            let path = stream.next().ok_or(ToolError::Path)?.map_err(|error| {
                if error.is_eof() {
                    ToolError::UnterminatedQuotedPath.at_path(tail)
                } else {
                    ToolError::Path.at_path(tail)
                }
            })?;
            (path, stream.byte_offset())
        } else {
            let used = tail.find(char::is_whitespace).unwrap_or(tail.len());
            (tail[..used].to_owned(), used)
        };
        if !path.is_empty() {
            super::types::validate_mention_path(path.strip_suffix('/').unwrap_or(&path), false)
                .map_err(|error| error.at_path(&path))?;
            let path = PathBuf::from(path);
            if !paths.contains(&path) {
                paths.push(path);
            }
            if paths.len() > crate::session::input::MAX_INCLUDES {
                return Err(ToolError::Limit);
            }
        }
        remaining = &tail[used..];
    }
    Ok(paths)
}

#[derive(Clone)]
pub struct WorkspacePermissions {
    path: PathBuf,
    identity: WorkspaceIdentity,
    trusted: bool,
    mode: ApprovalMode,
    runtime_grants: Vec<RuntimeGrant>,
    candidates: Vec<Candidate>,
    discovery_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RuntimeProfileOption {
    pub token: String,
    pub label: String,
    pub group: &'static str,
    pub version: Option<String>,
    pub path: PathBuf,
    pub detected: bool,
    pub selected: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrustRecordError {
    #[error("workspace-permissions.json uses unsupported version {0}; this Arany reads version 2")]
    UnsupportedVersion(u64),
    #[error("workspace-permissions.json is malformed")]
    Malformed,
    #[error(
        "workspace-permissions.json is not a private owner-only regular file within 64 KiB; fix or remove it"
    )]
    RecordNotPrivate,
    #[error("the Arany state directory is not private; fix its permissions")]
    StateNotPrivate,
    #[error("the Arany state directory is inside this folder; move it or open another folder")]
    StateInsideWorkspace,
    #[error("the Arany state directory cannot be opened; check its permissions")]
    StateUnavailable,
    #[error("this folder cannot be opened by its absolute path")]
    Workspace,
    #[error("workspace-permissions.json changed during recovery; retry")]
    Changed,
}

impl TrustRecordError {
    pub fn is_resettable(&self) -> bool {
        matches!(self, Self::UnsupportedVersion(_) | Self::Malformed)
    }
}

impl From<TrustRecordError> for ToolError {
    fn from(_: TrustRecordError) -> Self {
        ToolError::Configuration
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Permissions {
    version: u8,
    workspaces: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: PathBuf,
    device: u64,
    inode: u64,
    mode: ApprovalMode,
    runtime_grants: Vec<RuntimeGrant>,
}

fn record_error(error: StoreError) -> TrustRecordError {
    match error {
        // Only a symlink fails to open; FIFOs and directories fail the privacy check instead.
        StoreError::StateNotPrivate | StoreError::Io(_) => TrustRecordError::RecordNotPrivate,
        _ => TrustRecordError::StateUnavailable,
    }
}

fn state_error(error: StoreError) -> TrustRecordError {
    match error {
        StoreError::StateNotPrivate => TrustRecordError::StateNotPrivate,
        _ => TrustRecordError::StateUnavailable,
    }
}

pub(crate) const WORKSPACE_PERMISSIONS: RecordFormat = RecordFormat {
    kind: RecordKind::WorkspacePermissions,
    field: "version",
    floor: 2,
    current: 2,
    steps: &[],
    accepts: |bytes| parse(bytes).is_ok(),
};

fn read_bytes(root: &StateRoot) -> Result<Option<Vec<u8>>, TrustRecordError> {
    root.read_current(&WORKSPACE_PERMISSIONS)
        .map_err(record_error)
}

fn read(root: &StateRoot) -> Result<Permissions, TrustRecordError> {
    match read_bytes(root)? {
        Some(bytes) => parse(&bytes),
        None => Ok(Permissions {
            version: 2,
            workspaces: Vec::new(),
        }),
    }
}

fn parse(bytes: &[u8]) -> Result<Permissions, TrustRecordError> {
    let value =
        super::skills::parse_json(bytes, 64 * 1024).map_err(|_| TrustRecordError::Malformed)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(TrustRecordError::Malformed)?;
    if version != 2 {
        return Err(TrustRecordError::UnsupportedVersion(version));
    }
    let record: Permissions =
        serde_json::from_value(value).map_err(|_| TrustRecordError::Malformed)?;
    if record.workspaces.len() > 128
        || record.workspaces.iter().any(|entry| {
            !entry.path.is_absolute()
                || entry.path.as_os_str().len() > MAX_PATH_BYTES
                || entry.inode == 0
                || validate_selection(&entry.runtime_grants).is_err()
        })
    {
        return Err(TrustRecordError::Malformed);
    }
    Ok(record)
}

impl WorkspacePermissions {
    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discovery failure that left the chooser with saved profiles only.
    pub fn discovery_error(&self) -> Option<&str> {
        self.discovery_error.as_deref()
    }

    pub fn runtime_profile_choices(&self) -> Vec<RuntimeProfileOption> {
        let selected = default_profile_selection(&self.candidates, &self.runtime_grants);
        let mut profiles = self
            .candidates
            .iter()
            .map(|candidate| {
                (
                    &candidate.grant,
                    candidate.token.clone(),
                    true,
                    candidate.label.clone(),
                )
            })
            .collect::<Vec<_>>();
        for grant in &self.runtime_grants {
            let Ok(token) = grant.token() else {
                continue;
            };
            if profiles
                .iter()
                .any(|(_, existing, _, _)| *existing == token)
            {
                continue;
            }
            let detected = grant.verify_host().is_ok();
            profiles.push((grant, token, detected, saved_profile_label(grant, detected)));
        }
        profiles.sort_by(|left, right| profile_order((left.0, &left.1), (right.0, &right.1)));
        profiles
            .into_iter()
            .map(|(grant, token, detected, label)| RuntimeProfileOption {
                selected: selected.contains(&token),
                token,
                label,
                group: grant.kind.label(),
                version: grant.version.clone(),
                path: grant.path.clone(),
                detected,
            })
            .collect()
    }

    /// Labels of every saved profile, for the inspection notice; a drifted one is marked.
    pub fn selected_runtime_profiles(&self) -> Vec<String> {
        self.runtime_grants
            .iter()
            .map(|grant| saved_profile_label(grant, grant.verify_host().is_ok()))
            .collect()
    }

    /// Startup notices for saved grants that no longer verify; they stay pinned, so commands using them are rejected.
    pub fn stale_runtime_notices(&self) -> Vec<String> {
        self.runtime_grants
            .iter()
            .filter(|grant| grant.verify_host().is_err())
            .map(|grant| {
                format!(
                    "Runtime profile {} no longer matches; commands using it are rejected until you remove it in /permissions",
                    grant.kind.label()
                )
            })
            .collect()
    }

    pub(crate) fn matches_identity(&self, identity: WorkspaceIdentity) -> bool {
        self.identity == identity
    }

    pub fn open(path: &Path) -> Result<Self, TrustRecordError> {
        let account = StateRoot::account_path().map_err(state_error)?;
        match StateRoot::open_existing(&account) {
            Ok(root) => Self::load(path, &root),
            // Reading never creates State; without a History database no trust can be remembered.
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let directory =
                    fs::open_directory(path).map_err(|_| TrustRecordError::Workspace)?;
                let identity =
                    fs::workspace_identity(&directory).map_err(|_| TrustRecordError::Workspace)?;
                fs::reject_overlap(path, &account)
                    .map_err(|_| TrustRecordError::StateInsideWorkspace)?;
                Ok(Self {
                    path: path.to_owned(),
                    identity,
                    trusted: false,
                    mode: ApprovalMode::AutoEdits,
                    runtime_grants: Vec::new(),
                    candidates: Vec::new(),
                    discovery_error: None,
                })
            }
            Err(error) => Err(state_error(error)),
        }
    }

    /// Discovery failure keeps the saved profiles listed and is reported with the chooser.
    pub fn prepare_profile_selection(self) -> Self {
        self.with_candidates(discover_home_candidates())
    }

    fn with_candidates(mut self, discovered: Result<Vec<Candidate>, ToolError>) -> Self {
        match discovered {
            Ok(candidates) => self.candidates = candidates,
            Err(error) => self.discovery_error = Some(error.to_string()),
        }
        self
    }

    fn load(path: &Path, root: &StateRoot) -> Result<Self, TrustRecordError> {
        let directory = fs::open_directory(path).map_err(|_| TrustRecordError::Workspace)?;
        let identity =
            fs::workspace_identity(&directory).map_err(|_| TrustRecordError::Workspace)?;
        root.ensure_outside_workspace(&directory)
            .map_err(|_| TrustRecordError::StateInsideWorkspace)?;
        let record = read(root)?;
        let entry = record.workspaces.iter().find(|entry| {
            entry.path == path
                && WorkspaceIdentity {
                    device: entry.device,
                    inode: entry.inode,
                } == identity
        });
        let runtime_grants = entry.map_or_else(Vec::new, |entry| entry.runtime_grants.clone());
        let trusted = entry.is_some();
        Ok(Self {
            path: path.to_owned(),
            identity,
            trusted,
            mode: entry.map_or(ApprovalMode::AutoEdits, |entry| entry.mode),
            runtime_grants,
            candidates: Vec::new(),
            discovery_error: None,
        })
    }

    pub fn remember_profiles(
        &mut self,
        trusted: bool,
        mode: ApprovalMode,
        tokens: &[String],
    ) -> Result<(), ToolError> {
        let account = StateRoot::account_path().map_err(|_| ToolError::Configuration)?;
        let root = StateRoot::admit(&account).map_err(|_| ToolError::Configuration)?;
        self.remember_with(&root, trusted, mode, tokens)
    }

    fn remember_with(
        &mut self,
        root: &StateRoot,
        trusted: bool,
        mode: ApprovalMode,
        tokens: &[String],
    ) -> Result<(), ToolError> {
        if !trusted && !tokens.is_empty() {
            return Err(ToolError::Configuration);
        }
        let runtime_grants = self.selected_grants(tokens)?;
        for grant in &runtime_grants {
            grant.reject_overlap(&[self.path.as_path(), root.path()])?;
        }
        self.save(root, trusted, mode, runtime_grants)
    }

    fn selected_grants(&self, tokens: &[String]) -> Result<Vec<RuntimeGrant>, ToolError> {
        if tokens.len() > MAX_RUNTIME_GRANTS {
            return Err(ToolError::Limit);
        }
        let mut runtime_grants = Vec::new();
        for token in tokens {
            let grant = match self
                .candidates
                .iter()
                .find(|candidate| &candidate.token == token)
            {
                Some(candidate) => {
                    candidate.grant.verify_host()?;
                    candidate.grant.clone()
                }
                None => self
                    .runtime_grants
                    .iter()
                    .find(|grant| grant.token().ok().as_deref() == Some(token.as_str()))
                    .cloned()
                    .ok_or(ToolError::Configuration)?,
            };
            runtime_grants.push(grant);
        }
        validate_selection(&runtime_grants)?;
        Ok(runtime_grants)
    }

    fn save(
        &mut self,
        root: &StateRoot,
        trusted: bool,
        mode: ApprovalMode,
        runtime_grants: Vec<RuntimeGrant>,
    ) -> Result<(), ToolError> {
        root.with_account_replacement_lock(&self.path, || {
            self.verify(&self.path)?;
            let mut record = read(root)?;
            record.workspaces.retain(|entry| entry.path != self.path);
            if trusted {
                if record.workspaces.len() >= 128 {
                    return Err(ToolError::Limit);
                }
                record.workspaces.push(Entry {
                    path: self.path.clone(),
                    device: self.identity.device,
                    inode: self.identity.inode,
                    mode,
                    runtime_grants: if trusted {
                        runtime_grants.clone()
                    } else {
                        Vec::new()
                    },
                });
            }
            let bytes = serde_json::to_vec(&record).map_err(|_| ToolError::Configuration)?;
            root.record(RecordKind::WorkspacePermissions)
                .replace(&bytes)
                .map_err(|_| ToolError::Configuration)
        })
        .map_err(|_| ToolError::Configuration)??;
        self.trusted = trusted;
        self.mode = mode;
        self.runtime_grants = if trusted { runtime_grants } else { Vec::new() };
        Ok(())
    }

    pub(crate) fn verify(&self, path: &Path) -> Result<(), ToolError> {
        if path != self.path || fs::workspace_identity(&fs::open_directory(path)?)? != self.identity
        {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }

    pub(crate) fn config(&self, path: &Path) -> Result<Config, ToolError> {
        self.verify(path)?;
        if !self.trusted {
            return Err(ToolError::Configuration);
        }
        let commands = {
            let shell = &super::executable::EXECUTABLES.shell;
            let executable =
                std::fs::canonicalize(shell.path).map_err(|_| ToolError::ProtectionUnavailable)?;
            if !executable.starts_with(shell.root) {
                return Err(ToolError::ProtectionUnavailable);
            }
            let executable = executable.to_str().ok_or(ToolError::Path)?.to_owned();
            let bytes = fs::read_regular(fs::open_absolute(&executable)?, MAX_SNAPSHOT_BYTES)?;
            vec![Program {
                name: "sh".into(),
                executable,
                sha256: hex_digest(&bytes),
                interpreter: true,
                inputs: Vec::new(),
            }]
        };
        let config = Config {
            version: 1,
            workspace_paths: vec![".".into()],
            write: true,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
            child_web: false,
            commands,
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants: self.runtime_grants.clone(),
            git: super::git::discover(),
            git_identity: super::git::user_identity(),
            locale: Default::default(),
            environment_sha256: None,
            network: None,
            web_search: None,
        };
        config.validate()?;
        Ok(config)
    }
}

/// Pins the current unsupported or malformed record so a consented reset deletes exactly it.
pub struct TrustReset {
    root: StateRoot,
    workspace: PathBuf,
    bytes: Vec<u8>,
}

impl WorkspacePermissions {
    pub fn prepare_reset(workspace: &Path) -> Result<TrustReset, TrustRecordError> {
        let account = StateRoot::account_path().map_err(state_error)?;
        let root = StateRoot::open_existing(&account).map_err(state_error)?;
        prepare_reset_with(workspace, root)
    }
}

fn prepare_reset_with(workspace: &Path, root: StateRoot) -> Result<TrustReset, TrustRecordError> {
    let bytes = read_bytes(&root)?.ok_or(TrustRecordError::Changed)?;
    match parse(&bytes) {
        Err(error) if error.is_resettable() => Ok(TrustReset {
            root,
            workspace: workspace.to_owned(),
            bytes,
        }),
        Err(error) => Err(error),
        Ok(_) => Err(TrustRecordError::Changed),
    }
}

impl TrustReset {
    pub fn apply(self) -> Result<(), TrustRecordError> {
        self.root
            .with_account_replacement_lock(&self.workspace, || {
                let current = read_bytes(&self.root)?;
                if current.as_deref() != Some(self.bytes.as_slice()) {
                    return Err(TrustRecordError::Changed);
                }
                self.root
                    .record(RecordKind::WorkspacePermissions)
                    .remove()
                    .map_err(record_error)
            })
            .map_err(state_error)?
    }
}

fn saved_profile_label(grant: &RuntimeGrant, detected: bool) -> String {
    if detected {
        grant.profile_label()
    } else {
        format!("{} (no longer detected)", grant.profile_label())
    }
}

fn discover_home_candidates() -> Result<Vec<Candidate>, ToolError> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(Vec::new());
    };
    if !home.is_absolute() {
        return Ok(Vec::new());
    }
    discover(&home)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE_CARGO_NOTICE: &str = "Runtime profile Cargo archive cache no longer matches; commands using it are rejected until you remove it in /permissions";

    #[test]
    fn runtime_profile_numbers_follow_kind_version_path_and_token() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let mut ordered = [
            (RuntimeKind::Rust, Some("1.9.0"), "/rust", "z"),
            (RuntimeKind::Rust, Some("1.10.0"), "/rust", "y"),
            (RuntimeKind::Node, Some("v22.3.0"), "/a", "x"),
            (RuntimeKind::Node, Some("v22.3.0"), "/b", "v"),
            (RuntimeKind::Node, Some("v22.3.0"), "/b", "w"),
            (RuntimeKind::Python, Some("3.13.0"), "/python", "u"),
            (RuntimeKind::CargoCache, None, "/cache", "t"),
            (RuntimeKind::AppleCompiler, None, "/compiler", "s"),
            (RuntimeKind::AppleSdk, Some("15.9"), "/sdk", "r"),
            (RuntimeKind::AppleSdk, Some("15.10.0"), "/sdk", "q"),
        ]
        .map(|(kind, version, path, token)| {
            let grant = RuntimeGrant {
                kind,
                manager: RuntimeManager::HostCache,
                version: version.map(str::to_owned),
                path: path.into(),
                device: 1,
                inode: 2,
            };
            Candidate {
                label: grant.profile_label(),
                token: token.into(),
                grant,
            }
        });
        for index in [0, 8] {
            ordered[index].token = ordered[index].grant.token().unwrap();
        }
        let expected = ordered
            .iter()
            .map(|candidate| candidate.token.clone())
            .collect::<Vec<_>>();
        for shift in 0..ordered.len() {
            let mut candidates = ordered.to_vec();
            candidates.rotate_left(shift);
            candidates.reverse();
            let access = WorkspacePermissions {
                path: "/workspace".into(),
                identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
                trusted: true,
                mode: ApprovalMode::AutoEdits,
                runtime_grants: vec![ordered[0].grant.clone()],
                candidates,
                discovery_error: None,
            };
            assert_eq!(
                access
                    .runtime_profile_choices()
                    .into_iter()
                    .map(|choice| choice.token)
                    .collect::<Vec<_>>(),
                expected,
                "shuffle={shift}"
            );
            assert_eq!(
                access
                    .runtime_profile_choices()
                    .into_iter()
                    .filter(|choice| choice.selected)
                    .map(|choice| choice.token)
                    .collect::<Vec<_>>(),
                [0, 2, 5, 6, 7, 9].map(|index| ordered[index].token.clone()),
                "saved selection follows its token after shuffle={shift}"
            );
            let mut access = access;
            access.runtime_grants.push(ordered[8].grant.clone());
            assert!(
                access
                    .runtime_profile_choices()
                    .iter()
                    .any(|choice| choice.selected && choice.token == ordered[8].token)
            );
            access.runtime_grants.clear();
            assert_eq!(
                access
                    .runtime_profile_choices()
                    .into_iter()
                    .filter(|choice| choice.selected)
                    .map(|choice| choice.token)
                    .collect::<Vec<_>>(),
                [1, 2, 5, 6, 7, 9].map(|index| ordered[index].token.clone())
            );
            access.candidates.retain(|candidate| {
                !matches!(
                    candidate.grant.kind,
                    RuntimeKind::Rust | RuntimeKind::Node | RuntimeKind::Python
                )
            });
            assert!(
                access
                    .runtime_profile_choices()
                    .iter()
                    .all(|choice| !choice.selected)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn runtime_discovery_is_bounded_metadata_only_and_pins_exact_versions() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager, discover};
        use std::os::unix::fs::PermissionsExt;

        fn executable(path: &Path) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"synthetic executable marker").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let rust = home.join(".rustup/toolchains/1.88.0-x86_64-unknown-linux-gnu");
        executable(&rust.join("bin/cargo"));
        executable(&rust.join("bin/rustc"));
        let channel_alias = home.join(".rustup/toolchains/stable");
        executable(&channel_alias.join("bin/cargo"));
        executable(&channel_alias.join("bin/rustc"));
        for invalid in [
            "01.2.3",
            "1.2",
            "1.2.3-",
            "1.2.3/escape",
            "18446744073709551616.0.0",
        ] {
            let path = home.join(".rustup/toolchains").join(invalid);
            executable(&path.join("bin/cargo"));
            executable(&path.join("bin/rustc"));
        }
        let node = home.join(".nvm/versions/node/v22.3.0");
        executable(&node.join("bin/node"));
        let python_alias = home.join(".pyenv/versions/3.13.0");
        executable(&python_alias.join("bin/python3.13"));
        std::os::unix::fs::symlink("python3.13", python_alias.join("bin/python3")).unwrap();
        let python_regular = home.join(".pyenv/versions/3.13.1");
        executable(&python_regular.join("bin/python3.13"));
        let python_escape = home.join(".pyenv/versions/3.13.2");
        std::fs::create_dir_all(python_escape.join("bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", python_escape.join("bin/python3")).unwrap();
        let cache = home.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        for part in ["src", "index"] {
            std::fs::create_dir_all(home.join(".cargo/registry").join(part)).unwrap();
        }
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(
            home.join(".cargo/config.toml"),
            "synthetic credential canary",
        )
        .unwrap();

        let candidates = discover(&home).unwrap();
        let rust = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::Rust)
            .unwrap();
        assert_eq!(rust.grant.manager, RuntimeManager::Rustup);
        assert_eq!(
            rust.grant.version.as_deref(),
            Some("1.88.0-x86_64-unknown-linux-gnu")
        );
        assert!(
            rust.grant
                .path
                .ends_with(".rustup/toolchains/1.88.0-x86_64-unknown-linux-gnu")
        );
        let node = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::Node)
            .unwrap();
        assert_eq!(node.grant.version.as_deref(), Some("v22.3.0"));
        let python = candidates
            .iter()
            .filter(|candidate| candidate.grant.kind == RuntimeKind::Python)
            .collect::<Vec<_>>();
        assert_eq!(python.len(), 2);
        assert!(
            python
                .iter()
                .any(|candidate| candidate.grant.path == python_alias)
        );
        assert!(
            python
                .iter()
                .any(|candidate| candidate.grant.path == python_regular)
        );
        assert!(
            python
                .iter()
                .all(|candidate| candidate.grant.path != python_escape)
        );
        assert!(candidates.iter().all(|candidate| {
            !candidate.label.contains("synthetic credential canary")
                && candidate.grant.path != home.join(".cargo")
                && candidate.grant.path != channel_alias
                && (candidate.grant.kind != RuntimeKind::Rust || candidate.grant == rust.grant)
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.grant.kind == RuntimeKind::CargoCache && candidate.grant.path == cache
        }));
        #[cfg(target_os = "macos")]
        {
            use super::super::toolchains::RuntimeManager;

            let compilers = candidates
                .iter()
                .filter(|candidate| candidate.grant.kind == RuntimeKind::AppleCompiler)
                .collect::<Vec<_>>();
            assert!(compilers.iter().all(|compiler| {
                compiler.grant.manager == RuntimeManager::AppleCommandLineTools
                    && compiler.grant.path == Path::new("/Library/Developer/CommandLineTools/usr")
                    && compiler.grant.verify_host().is_ok()
            }));
            let sdks = candidates
                .iter()
                .filter(|candidate| candidate.grant.kind == RuntimeKind::AppleSdk)
                .collect::<Vec<_>>();
            assert!(sdks.iter().all(|candidate| {
                !candidate.grant.kind.is_toolchain()
                    && !candidate.grant.kind.is_cache()
                    && candidate.grant.kind.sandbox_cache_path().is_none()
                    && candidate.grant.path.is_dir()
                    && candidate
                        .grant
                        .path
                        .extension()
                        .is_some_and(|extension| extension == "sdk")
            }));
            for sdk in sdks {
                sdk.grant.verify_host().unwrap();
            }
        }
        #[cfg(not(target_os = "macos"))]
        assert!(candidates.iter().all(|candidate| {
            !matches!(
                candidate.grant.kind,
                RuntimeKind::AppleCompiler | RuntimeKind::AppleSdk
            )
        }));
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("rust-toolchain.toml"),
            r#"[toolchain]
channel = "stable"
"#,
        )
        .unwrap();
        let mut access = WorkspacePermissions::load(&workspace, &state)
            .unwrap()
            .with_candidates(Ok(candidates.clone()));
        let choices = access.runtime_profile_choices();
        assert!(
            choices
                .iter()
                .any(|choice| choice.selected && choice.path == python_regular)
        );
        assert!(access.runtime_grants.is_empty(), "proposals grant nothing");
        let proposed = choices
            .iter()
            .filter(|choice| choice.selected)
            .map(|choice| choice.token.clone())
            .collect::<Vec<_>>();
        access
            .remember_with(&state, true, ApprovalMode::Request, &proposed)
            .unwrap();
        let saved = WorkspacePermissions::load(&workspace, &state).unwrap();
        assert_eq!(saved.runtime_grants.len(), proposed.len());
        assert!(saved.candidates.is_empty(), "startup does not rediscover");
        for kind in [
            RuntimeKind::CargoCache,
            RuntimeKind::CargoSource,
            RuntimeKind::CargoIndex,
        ] {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.grant.kind == kind)
                .unwrap();
            access
                .remember_with(
                    &state,
                    true,
                    ApprovalMode::Request,
                    std::slice::from_ref(&candidate.token),
                )
                .unwrap();
            let saved = WorkspacePermissions::load(&workspace, &state).unwrap();
            assert_eq!(
                saved.runtime_grants.as_slice(),
                std::slice::from_ref(&candidate.grant)
            );
            assert!(
                saved
                    .config(&workspace)
                    .unwrap()
                    .runtime_grants
                    .iter()
                    .all(|grant| grant.kind == kind)
            );
        }
        rust.grant.verify_host().unwrap();

        let mut wrong_version = rust.grant.clone();
        wrong_version.version = Some("stable".into());
        assert!(wrong_version.validate_shape().is_err());
        let mut duplicate_kind = vec![rust.grant.clone(), rust.grant.clone()];
        assert!(validate_selection(&duplicate_kind).is_err());
        duplicate_kind.truncate(1);
        validate_selection(&duplicate_kind).unwrap();

        let replaced = rust.grant.clone();
        std::fs::rename(&replaced.path, home.join("old-rust")).unwrap();
        std::fs::create_dir_all(&replaced.path).unwrap();
        assert!(matches!(
            replaced.verify_host(),
            Err(ToolError::ChangedInput)
        ));
        let newer = home.join(".rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu");
        executable(&newer.join("bin/cargo"));
        executable(&newer.join("bin/rustc"));
        let mut saved = saved.with_candidates(discover(&home));
        let choices = saved.runtime_profile_choices();
        assert!(choices.iter().any(|choice| choice.selected
            && !choice.detected
            && choice.token == replaced.token().unwrap()));
        let newer_token = choices
            .iter()
            .find(|choice| choice.path == newer)
            .unwrap()
            .token
            .clone();
        saved
            .remember_with(&state, true, ApprovalMode::Request, &[newer_token])
            .unwrap();
        assert_eq!(
            WorkspacePermissions::load(&workspace, &state)
                .unwrap()
                .runtime_grants[0]
                .path,
            newer
        );
    }

    #[test]
    fn apple_runtime_kinds_are_opt_in_and_fail_closed_outside_macos() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let compiler = RuntimeKind::AppleCompiler;
        let sdk = RuntimeKind::AppleSdk;
        assert!(compiler.is_toolchain());
        assert!(!compiler.is_cache());
        assert!(!sdk.is_toolchain());
        assert!(!sdk.is_cache());
        assert_eq!(sdk.sandbox_cache_path(), None);

        let grant = |kind, version: Option<&str>, path: &str| RuntimeGrant {
            kind,
            manager: RuntimeManager::AppleCommandLineTools,
            version: version.map(str::to_owned),
            path: PathBuf::from(path),
            device: 1,
            inode: 1,
        };
        let compiler = grant(compiler, None, "/Library/Developer/CommandLineTools/usr");
        let sdk = grant(
            sdk,
            Some("15.4"),
            "/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk",
        );

        #[cfg(target_os = "macos")]
        {
            validate_selection(&[compiler.clone(), sdk.clone()]).unwrap();
            assert!(validate_selection(&[sdk.clone(), sdk.clone()]).is_err());
            for version in ["15", "15.04", "015.4", "15.4.0.1", "999999.4"] {
                let malformed = grant(
                    RuntimeKind::AppleSdk,
                    Some(version),
                    &format!("/Library/Developer/CommandLineTools/SDKs/MacOSX{version}.sdk"),
                );
                assert!(validate_selection(&[malformed]).is_err());
            }
            let alias = grant(
                RuntimeKind::AppleSdk,
                Some("15.0"),
                "/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk",
            );
            assert!(validate_selection(&[alias]).is_err());
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(validate_selection(&[compiler, sdk]).is_err());
        }
    }

    #[test]
    fn explicit_mentions_and_file_discovery_exclude_unsafe_inputs() {
        for (objective, expected) in [
            ("Edit @README.md", vec!["README.md"]),
            (
                "Edit @\"docs/hello world.md\" and @README.md @README.md",
                vec!["docs/hello world.md", "README.md"],
            ),
            ("user@example.com @@README.md", vec![]),
            ("@中.md", vec!["中.md"]),
            (
                "Inspect @docs/ and @\"docs/hello world/\" then @README.md",
                vec!["docs", "docs/hello world", "README.md"],
            ),
            ("@docs @docs/ @\"docs/\"", vec!["docs"]),
            ("@\"docs/hello world\"", vec!["docs/hello world"]),
        ] {
            assert_eq!(
                mention_paths(objective).unwrap(),
                expected.iter().map(PathBuf::from).collect::<Vec<_>>()
            );
        }
        for objective in [
            "@../outside",
            "@/absolute",
            "@.env",
            "@\"unfinished",
            "@.git/config",
            "@../outside/",
            "@/",
            "@.env/",
            "@docs//",
        ] {
            assert!(mention_paths(objective).is_err());
        }
        assert_eq!(
            mention_paths("@../outside").unwrap_err().to_string(),
            "\"../outside\": path is outside workspace"
        );
        assert_eq!(
            mention_paths("@.git").unwrap_err().to_string(),
            "\".git\": protected path"
        );
        for tail in ["\"unfinished", "\"docs/with spaces/", "\"docs/trailing\\"] {
            assert_eq!(
                mention_paths(&format!("Inspect @README.md @{tail}"))
                    .unwrap_err()
                    .to_string(),
                format!("{tail:?}: unterminated quoted path"),
            );
        }
        assert_eq!(
            mention_paths("Inspect @README.md @\".ENV/\"")
                .unwrap_err()
                .to_string(),
            "\".ENV/\": protected path",
        );
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::create_dir(workspace.join("target")).unwrap();
        std::fs::write(workspace.join("README.md"), "synthetic file").unwrap();
        std::fs::write(workspace.join("docs/hello world.md"), "synthetic file").unwrap();
        std::fs::write(workspace.join(".env"), "synthetic excluded fixture").unwrap();
        std::os::unix::fs::symlink(temp.path(), workspace.join("outside")).unwrap();
        std::fs::hard_link(workspace.join("README.md"), workspace.join("linked.md")).unwrap();
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            assert_eq!(
                fs::workspace_entries(&workspace, "", "").unwrap(),
                vec!["docs/"]
            );
            let nested = fs::workspace_entries(&workspace, "docs", "hello");
            assert!(
                matches!(&nested, Err(ToolError::ProtectionUnavailable)),
                "{nested:?}"
            );
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert_eq!(
                fs::workspace_entries(&workspace, "", "").unwrap(),
                vec!["docs/"]
            );
            assert_eq!(
                fs::workspace_entries(&workspace, "docs", "hello").unwrap(),
                vec!["docs/hello world.md"]
            );
            assert_eq!(
                fs::workspace_entries(&workspace, "docs", "").unwrap(),
                vec!["docs/hello world.md"]
            );
            for (folder, reason) in [
                ("missing", "Tool file or directory not found"),
                ("../outside", "\"../outside\": path is outside workspace"),
                (".git", "\".git\": protected path"),
            ] {
                assert_eq!(
                    fs::workspace_entries(&workspace, folder, "")
                        .unwrap_err()
                        .to_string(),
                    reason
                );
            }
            for query in ["world", "HELLO", "docs/hello", "hwd"] {
                assert_eq!(
                    fs::workspace_entries(&workspace, "", query).unwrap(),
                    vec!["docs/hello world.md"],
                    "nested file query {query}"
                );
            }
            std::fs::create_dir_all(workspace.join("docs/guide")).unwrap();
            std::fs::write(workspace.join("docs/guide/intro.md"), "synthetic file").unwrap();
            std::fs::write(workspace.join("guide"), "synthetic file").unwrap();
            std::fs::write(
                workspace.join("target/guide.md"),
                "synthetic excluded fixture",
            )
            .unwrap();
            assert_eq!(
                fs::workspace_entries(&workspace, "", "guide").unwrap(),
                vec!["guide", "docs/guide/", "docs/guide/intro.md"]
            );
            assert!(
                fs::workspace_entries(&workspace, "", "README")
                    .unwrap()
                    .is_empty()
            );
            for query in ["../outside", ".env", "docs/../../outside", "bad\\path"] {
                assert!(fs::workspace_entries(&workspace, "", query).is_err());
            }
            for index in 0..70 {
                std::fs::write(
                    workspace.join(format!("entry-{index:03}.md")),
                    "synthetic file",
                )
                .unwrap();
            }
            assert_eq!(
                fs::workspace_entries(&workspace, "", "entry-").unwrap(),
                (0..64)
                    .map(|index| format!("entry-{index:03}.md"))
                    .collect::<Vec<_>>()
            );
            let large = workspace.join("large");
            std::fs::create_dir(&large).unwrap();
            for index in 0..8193 {
                std::fs::write(large.join(format!("item-{index:05}.md")), b"synthetic").unwrap();
            }
            assert!(matches!(
                fs::workspace_entries(&workspace, "large", ""),
                Err(ToolError::Limit)
            ));
            for folder in ["../", "outside", ".git", ".env"] {
                assert!(fs::workspace_entries(&workspace, folder, "").is_err());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn stale_runtime_grant_stays_pinned_for_rejection_and_removal() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let cache = temp.path().join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let (device, inode) = fs::identity(&fs::open_directory(&cache).unwrap()).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::CargoCache,
            manager: RuntimeManager::HostCache,
            version: None,
            path: cache,
            device,
            inode,
        };
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        access
            .save(&root, true, ApprovalMode::AutoEdits, vec![grant])
            .unwrap();
        assert_eq!(access.config(&path).unwrap().runtime_grants.len(), 1);

        std::fs::remove_dir_all(temp.path().join(".cargo")).unwrap();
        let restored = WorkspacePermissions::load(&path, &root).unwrap();
        assert!(restored.is_trusted());
        assert_eq!(restored.stale_runtime_notices(), [STALE_CARGO_NOTICE]);
        assert_eq!(restored.config(&path).unwrap().runtime_grants.len(), 1);
        let choices = restored.runtime_profile_choices();
        assert_eq!(choices.len(), 1);
        assert!(choices[0].selected);
        assert!(choices[0].label.ends_with("(no longer detected)"));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_grant_identity_drift_fails_verification_and_changes_token() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let grant_at = |path: &Path| {
            let (device, inode) = fs::identity(&fs::open_directory(path).unwrap()).unwrap();
            RuntimeGrant {
                kind: RuntimeKind::CargoCache,
                manager: RuntimeManager::HostCache,
                version: None,
                path: path.to_owned(),
                device,
                inode,
            }
        };
        let original = grant_at(&cache);
        original.verify_host().unwrap();
        let original_token = original.token().unwrap();

        // Keep the old directory alive while the path is recreated, so the new inode cannot be reused.
        std::fs::rename(&cache, temp.path().join("previous-cache")).unwrap();
        std::fs::create_dir(&cache).unwrap();
        assert!(matches!(
            original.verify_host(),
            Err(ToolError::ChangedInput)
        ));
        assert_ne!(grant_at(&cache).token().unwrap(), original_token);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_failure_stays_local_to_the_chooser_and_keeps_saved_profiles() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let home = temp.path().join("home");
        let cache = home.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        for index in 0..129 {
            std::fs::create_dir_all(home.join(format!(".nvm/versions/node/v1.0.{index}"))).unwrap();
        }
        let discovered = discover(&home);
        assert!(matches!(discovered, Err(ToolError::Limit)));
        let (device, inode) = fs::identity(&fs::open_directory(&cache).unwrap()).unwrap();
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        access
            .save(
                &root,
                true,
                ApprovalMode::AutoEdits,
                vec![RuntimeGrant {
                    kind: RuntimeKind::CargoCache,
                    manager: RuntimeManager::HostCache,
                    version: None,
                    path: cache.clone(),
                    device,
                    inode,
                }],
            )
            .unwrap();

        let access = WorkspacePermissions::load(&path, &root)
            .unwrap()
            .with_candidates(discovered);
        assert!(access.is_trusted());
        assert!(access.discovery_error().is_some());
        let choices = access.runtime_profile_choices();
        assert_eq!(choices.len(), 1);
        assert!(choices[0].selected);
        assert!(choices[0].detected);
        assert_eq!(choices[0].path, cache);
        assert_eq!(choices[0].version, None);
        assert!(!choices[0].label.contains("no longer detected"));
    }

    #[cfg(unix)]
    #[test]
    fn confirming_unchanged_trust_keeps_a_stale_pin_without_reverifying() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let cargo = temp.path().join("home/.cargo");
        let cache = cargo.join("registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let (device, inode) = fs::identity(&fs::open_directory(&cache).unwrap()).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::CargoCache,
            manager: RuntimeManager::HostCache,
            version: None,
            path: cache.clone(),
            device,
            inode,
        };
        let token = grant.token().unwrap();
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        access
            .save(&root, true, ApprovalMode::AutoEdits, vec![grant])
            .unwrap();

        // Keep the old directory alive so the recreated cache gets a new inode.
        std::fs::rename(&cargo, temp.path().join("previous-cargo")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        assert_eq!(access.stale_runtime_notices(), [STALE_CARGO_NOTICE]);

        access
            .remember_with(&root, true, ApprovalMode::AutoEdits, &[token])
            .unwrap();
        let restored = WorkspacePermissions::load(&path, &root).unwrap();
        assert_eq!(restored.runtime_grants.len(), 1);
        assert_eq!(
            (
                restored.runtime_grants[0].device,
                restored.runtime_grants[0].inode
            ),
            (device, inode),
            "the saved pin keeps the original identity"
        );
        assert_eq!(restored.stale_runtime_notices(), [STALE_CARGO_NOTICE]);
    }

    #[cfg(unix)]
    #[test]
    fn remembering_a_grant_inside_the_workspace_is_rejected_and_keeps_the_record() {
        use super::super::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        // The workspace is the parent of the cache, so saving the grant would overlap it.
        let workspace = temp.path().join("home");
        let cache = workspace.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let (device, inode) = fs::identity(&fs::open_directory(&cache).unwrap()).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::CargoCache,
            manager: RuntimeManager::HostCache,
            version: None,
            path: cache,
            device,
            inode,
        };
        let token = grant.token().unwrap();
        let mut access = WorkspacePermissions::load(&workspace, &root).unwrap();
        access
            .save(&root, true, ApprovalMode::AutoEdits, Vec::new())
            .unwrap();
        let before = root
            .record(RecordKind::WorkspacePermissions)
            .read()
            .unwrap();
        assert!(
            before.is_some(),
            "the untouched record exists before the attempt"
        );
        access.candidates = vec![Candidate {
            token: token.clone(),
            label: grant.profile_label(),
            grant,
        }];

        let error = access
            .remember_with(&root, true, ApprovalMode::Request, &[token])
            .unwrap_err();
        assert!(matches!(error, ToolError::Path), "{error:?}");
        assert_eq!(
            root.record(RecordKind::WorkspacePermissions)
                .read()
                .unwrap(),
            before
        );
        assert!(
            WorkspacePermissions::load(&workspace, &root)
                .unwrap()
                .runtime_grants
                .is_empty()
        );
    }

    #[test]
    fn private_trust_tracks_directory_identity_and_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        assert!(!access.is_trusted());
        access
            .save(&root, true, ApprovalMode::AutoEdits, Vec::new())
            .unwrap();
        #[cfg(target_os = "macos")]
        {
            let config = access.config(&path).unwrap();
            assert!(config.write);
            assert_eq!(config.workspace_paths, ["."]);
            assert_eq!(config.commands.len(), 1);
            assert_eq!(config.commands[0].name, "sh");
            assert_eq!(config.commands[0].executable, "/bin/sh");
            config.commands[0].verify().unwrap();
            assert!(config.mcp.is_empty());
        }
        assert_eq!(
            access.config(&path).unwrap().git.map(|git| git.sha256),
            super::super::git::discover().map(|git| git.sha256)
        );
        let restored = WorkspacePermissions::load(&path, &root).unwrap();
        assert!(restored.is_trusted());
        assert_eq!(restored.mode(), ApprovalMode::AutoEdits);
        std::fs::rename(&path, temp.path().join("previous-project")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            !WorkspacePermissions::load(&path, &root)
                .unwrap()
                .is_trusted()
        );
        assert!(
            access
                .save(&root, true, ApprovalMode::Auto, Vec::new())
                .is_err()
        );
        let mut replacement = WorkspacePermissions::load(&path, &root).unwrap();
        replacement
            .save(&root, true, ApprovalMode::Request, Vec::new())
            .unwrap();
        replacement
            .save(&root, false, ApprovalMode::Request, Vec::new())
            .unwrap();
        assert!(
            !WorkspacePermissions::load(&path, &root)
                .unwrap()
                .is_trusted()
        );
        for (record, cause) in [
            (
                br#"{"version":1,"workspaces":[]}"#.as_slice(),
                TrustRecordError::UnsupportedVersion(1),
            ),
            (
                br#"{"version":2,"workspaces":[{"path":"/project","device":1,"inode":1,"mode":"bypass","runtime_grants":[]}] }"#.as_slice(),
                TrustRecordError::Malformed,
            ),
        ] {
            root.record(RecordKind::WorkspacePermissions).replace(record).unwrap();
            assert_eq!(WorkspacePermissions::load(&path, &root).err(), Some(cause));
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(root.path().join("workspace-permissions.json")).unwrap();
            std::os::unix::fs::symlink(
                temp.path().join("outside"),
                root.path().join("workspace-permissions.json"),
            )
            .unwrap();
            assert_eq!(
                WorkspacePermissions::load(&path, &root).err(),
                Some(TrustRecordError::RecordNotPrivate)
            );
        }
    }

    #[test]
    fn unsupported_trust_record_resets_only_with_the_pinned_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let file = state.join("workspace-permissions.json");
        let version_one = br#"{"version":1,"workspaces":[]}"#;
        root.record(RecordKind::WorkspacePermissions)
            .replace(version_one)
            .unwrap();

        let reset = prepare_reset_with(&path, root.try_clone().unwrap()).unwrap();
        root.record(RecordKind::WorkspacePermissions)
            .replace(br#"{"version":1,"workspaces":[] }"#)
            .unwrap();
        assert_eq!(reset.apply(), Err(TrustRecordError::Changed));
        assert!(file.exists());

        let reset = prepare_reset_with(&path, root.try_clone().unwrap()).unwrap();
        reset.apply().unwrap();
        assert!(!file.exists());
        assert!(
            !WorkspacePermissions::load(&path, &root)
                .unwrap()
                .is_trusted()
        );

        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        access
            .save(&root, true, ApprovalMode::AutoEdits, Vec::new())
            .unwrap();
        let before = std::fs::read(&file).unwrap();
        assert_eq!(
            prepare_reset_with(&path, root.try_clone().unwrap()).err(),
            Some(TrustRecordError::Changed)
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);

        assert!(TrustRecordError::UnsupportedVersion(1).is_resettable());
        assert!(TrustRecordError::Malformed.is_resettable());
        assert!(!TrustRecordError::RecordNotPrivate.is_resettable());
        assert!(!TrustRecordError::StateNotPrivate.is_resettable());
        assert!(!TrustRecordError::StateInsideWorkspace.is_resettable());
        assert!(!TrustRecordError::StateUnavailable.is_resettable());
        assert!(!TrustRecordError::Workspace.is_resettable());
        assert!(!TrustRecordError::Changed.is_resettable());
    }

    #[test]
    fn malformed_trust_record_is_named_without_echoing_its_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        root.record(RecordKind::WorkspacePermissions)
            .replace(b"{not json")
            .unwrap();
        let error = WorkspacePermissions::load(&path, &root).err().unwrap();
        assert_eq!(error, TrustRecordError::Malformed);
        assert_eq!(error.to_string(), "workspace-permissions.json is malformed");
        assert!(!error.to_string().contains("not json"));
    }

    #[cfg(unix)]
    #[test]
    fn oversize_trust_record_is_named_as_not_private_and_never_reset() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let record = root.path().join("workspace-permissions.json");
        std::fs::write(&record, vec![b' '; 64 * 1024 + 1]).unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = WorkspacePermissions::load(&path, &root).err().unwrap();
        assert_eq!(error, TrustRecordError::RecordNotPrivate);
        assert!(error.to_string().contains("within 64 KiB"));
        assert!(!error.is_resettable());
        assert!(prepare_reset_with(&path, root.try_clone().unwrap()).is_err());
        assert!(record.exists());
    }
}

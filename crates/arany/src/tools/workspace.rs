use super::approval::ApprovalMode;
use super::config::{Config, Program};
use super::toolchains::{
    Candidate, MAX_RUNTIME_GRANTS, RuntimeGrant, discover, validate_selection,
};
use super::types::{MAX_SNAPSHOT_BYTES, hex_digest};
use super::{ToolError, fs};
use crate::store::RecordKind;
use crate::store::{StateRoot, StoreError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn mention_paths(objective: &str) -> Result<Vec<PathBuf>, ToolError> {
    if objective.len() > 8 * 1024 {
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
            let path = stream
                .next()
                .ok_or(ToolError::Path)?
                .map_err(|_| ToolError::Path)?;
            (path, stream.byte_offset())
        } else {
            let used = tail.find(char::is_whitespace).unwrap_or(tail.len());
            (tail[..used].to_owned(), used)
        };
        if !path.is_empty() {
            let directory = path.ends_with('/');
            if !super::types::valid_relative(path.strip_suffix('/').unwrap_or(&path), false) {
                return Err(ToolError::Path);
            }
            if directory {
                remaining = &tail[used..];
                continue;
            }
            let path = PathBuf::from(path);
            if !paths.contains(&path) {
                paths.push(path);
            }
            if paths.len() > 16 {
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
    identity: (u64, u64),
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
    pub selected: bool,
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

fn read(root: &StateRoot) -> Result<Permissions, ToolError> {
    let Some(bytes) = root
        .record(RecordKind::WorkspacePermissions)
        .read()
        .map_err(|_| ToolError::Configuration)?
    else {
        return Ok(Permissions {
            version: 2,
            workspaces: Vec::new(),
        });
    };
    let value = super::skills::parse_json(&bytes, 64 * 1024)?;
    let record: Permissions =
        serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
    if record.version != 2
        || record.workspaces.len() > 128
        || record.workspaces.iter().any(|entry| {
            !entry.path.is_absolute()
                || entry.path.as_os_str().len() > 4096
                || entry.inode == 0
                || validate_selection(&entry.runtime_grants).is_err()
        })
    {
        return Err(ToolError::Configuration);
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
        let selected = self
            .runtime_grants
            .iter()
            .filter_map(|grant| grant.token().ok())
            .collect::<std::collections::BTreeSet<_>>();
        let mut options = self
            .candidates
            .iter()
            .map(|candidate| RuntimeProfileOption {
                token: candidate.token.clone(),
                label: candidate.label.clone(),
                group: candidate.grant.kind.label(),
                selected: selected.contains(&candidate.token),
            })
            .collect::<Vec<_>>();
        for grant in &self.runtime_grants {
            let Ok(token) = grant.token() else {
                continue;
            };
            if options.iter().any(|option| option.token == token) {
                continue;
            }
            options.push(RuntimeProfileOption {
                token,
                label: saved_profile_label(grant),
                group: grant.kind.label(),
                selected: true,
            });
        }
        options.sort_by(|left, right| left.token.cmp(&right.token));
        options
    }

    /// Labels of every saved profile, for the inspection notice; a drifted one is marked.
    pub fn selected_runtime_profiles(&self) -> Vec<String> {
        self.runtime_grants
            .iter()
            .map(saved_profile_label)
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

    pub(crate) fn matches_identity(&self, identity: (u64, u64)) -> bool {
        self.identity == identity
    }

    pub fn open(path: &Path) -> Result<Self, ToolError> {
        let account = StateRoot::account_path().map_err(|_| ToolError::Configuration)?;
        match StateRoot::open_existing(&account) {
            Ok(root) => Self::load(path, &root),
            // Reading never creates State; without a History database no trust can be remembered.
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let directory = fs::open_directory(path)?;
                let identity = fs::identity(&directory)?;
                fs::reject_overlap(path, &account)?;
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
            Err(_) => Err(ToolError::Configuration),
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

    fn load(path: &Path, root: &StateRoot) -> Result<Self, ToolError> {
        let directory = fs::open_directory(path)?;
        let identity = fs::identity(&directory)?;
        root.ensure_outside_workspace(&directory)
            .map_err(|_| ToolError::Configuration)?;
        let record = read(root)?;
        let entry = record
            .workspaces
            .iter()
            .find(|entry| entry.path == path && (entry.device, entry.inode) == identity);
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

    /// Fresh candidates are verified now; a saved grant keeps its pin and rejects dispatch on drift.
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
                    device: self.identity.0,
                    inode: self.identity.1,
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
        if path != self.path || fs::identity(&fs::open_directory(path)?)? != self.identity {
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
            commands,
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants: self.runtime_grants.clone(),
            git: super::git::discover(),
        };
        config.validate()?;
        Ok(config)
    }
}

fn saved_profile_label(grant: &RuntimeGrant) -> String {
    if grant.verify_host().is_ok() {
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
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.grant.kind == RuntimeKind::CargoCache && candidate.grant.path == cache
        }));
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
                vec!["README.md"],
            ),
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
            let partial = fs::workspace_entries(&workspace, "large", "").unwrap();
            assert_eq!(partial.len(), 64);
            assert!(partial.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(partial.iter().all(|path| path.starts_with("large/item-")));
            let deep = std::iter::repeat_n("level", 16)
                .collect::<Vec<_>>()
                .join("/");
            std::fs::create_dir_all(workspace.join(&deep)).unwrap();
            std::fs::write(
                workspace.join(&deep).join("depth-canary.md"),
                "synthetic file",
            )
            .unwrap();
            assert!(
                fs::workspace_entries(&workspace, "", "depth-canary")
                    .unwrap()
                    .is_empty()
            );
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
                    path: cache,
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
        for record in [br#"{"version":1,"workspaces":[]}"#.as_slice(), br#"{"version":2,"workspaces":[{"path":"/project","device":1,"inode":1,"mode":"bypass","runtime_grants":[]}] }"#.as_slice()] {
            root.record(RecordKind::WorkspacePermissions).replace(record).unwrap();
            assert!(WorkspacePermissions::load(&path, &root).is_err());
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(root.path().join("workspace-permissions.json")).unwrap();
            std::os::unix::fs::symlink(
                temp.path().join("outside"),
                root.path().join("workspace-permissions.json"),
            )
            .unwrap();
            assert!(WorkspacePermissions::load(&path, &root).is_err());
        }
    }
}

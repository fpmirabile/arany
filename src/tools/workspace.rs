use super::approval::ApprovalMode;
use super::config::{Config, Program};
use super::types::{MAX_SNAPSHOT_BYTES, hex_digest};
use super::{ToolError, fs};
use crate::store::StateRoot;
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
}

fn read(root: &StateRoot) -> Result<Permissions, ToolError> {
    let Some(bytes) = root
        .read_workspace_permissions()
        .map_err(|_| ToolError::Configuration)?
    else {
        return Ok(Permissions {
            version: 1,
            workspaces: Vec::new(),
        });
    };
    let value = super::skills::parse_json(&bytes, 64 * 1024)?;
    let record: Permissions =
        serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
    if record.version != 1
        || record.workspaces.len() > 128
        || record.workspaces.iter().any(|entry| {
            !entry.path.is_absolute() || entry.path.as_os_str().len() > 4096 || entry.inode == 0
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
    pub(crate) fn matches_identity(&self, identity: (u64, u64)) -> bool {
        self.identity == identity
    }
    pub fn open(path: &Path) -> Result<Self, ToolError> {
        let root =
            StateRoot::admit(&StateRoot::account_path().map_err(|_| ToolError::Configuration)?)
                .map_err(|_| ToolError::Configuration)?;
        Self::load(path, &root)
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
        Ok(Self {
            path: path.to_owned(),
            identity,
            trusted: entry.is_some(),
            mode: entry.map_or(ApprovalMode::AutoEdits, |entry| entry.mode),
        })
    }

    pub fn remember(&mut self, trusted: bool, mode: ApprovalMode) -> Result<(), ToolError> {
        let root =
            StateRoot::admit(&StateRoot::account_path().map_err(|_| ToolError::Configuration)?)
                .map_err(|_| ToolError::Configuration)?;
        self.save(&root, trusted, mode)
    }

    fn save(
        &mut self,
        root: &StateRoot,
        trusted: bool,
        mode: ApprovalMode,
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
                });
            }
            let bytes = serde_json::to_vec(&record).map_err(|_| ToolError::Configuration)?;
            root.replace_workspace_permissions(&bytes)
                .map_err(|_| ToolError::Configuration)
        })
        .map_err(|_| ToolError::Configuration)??;
        self.trusted = trusted;
        self.mode = mode;
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
        let executable =
            std::fs::canonicalize("/usr/bin/sh").map_err(|_| ToolError::ProtectionUnavailable)?;
        if !executable.starts_with("/usr") {
            return Err(ToolError::ProtectionUnavailable);
        }
        let executable = executable.to_str().ok_or(ToolError::Path)?.to_owned();
        let bytes = fs::read_regular(fs::open_absolute(&executable)?, MAX_SNAPSHOT_BYTES)?;
        let config = Config {
            version: 1,
            workspace_paths: vec![".".into()],
            write: true,
            commands: vec![Program {
                name: "sh".into(),
                executable,
                sha256: hex_digest(&bytes),
                interpreter: true,
                inputs: Vec::new(),
            }],
            skills: Vec::new(),
            mcp: Vec::new(),
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn private_trust_tracks_directory_identity_and_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let path = temp.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let mut access = WorkspacePermissions::load(&path, &root).unwrap();
        assert!(!access.is_trusted());
        access.save(&root, true, ApprovalMode::AutoEdits).unwrap();
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
        assert!(access.save(&root, true, ApprovalMode::Auto).is_err());
        let mut replacement = WorkspacePermissions::load(&path, &root).unwrap();
        replacement
            .save(&root, true, ApprovalMode::Request)
            .unwrap();
        replacement
            .save(&root, false, ApprovalMode::Request)
            .unwrap();
        assert!(
            !WorkspacePermissions::load(&path, &root)
                .unwrap()
                .is_trusted()
        );
        for record in [br#"{"version":2,"workspaces":[]}"#.as_slice(), br#"{"version":1,"workspaces":[{"path":"/project","device":1,"inode":1,"mode":"bypass"}]}"#.as_slice()] {
            root.replace_workspace_permissions(record).unwrap();
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

use super::{ToolError, fs};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const MAX_RUNTIME_GRANTS: usize = 9;
const MAX_CANDIDATES: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeKind {
    Rust,
    Node,
    Python,
    CargoCache,
    CargoSource,
    CargoIndex,
    NpmCache,
    PipHttpCache,
    PipWheelCache,
}

impl RuntimeKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Node => "Node",
            Self::Python => "Python",
            Self::CargoCache => "Cargo archive cache",
            Self::CargoSource => "Cargo source cache",
            Self::CargoIndex => "Cargo index cache",
            Self::NpmCache => "npm content cache",
            Self::PipHttpCache => "pip HTTP cache",
            Self::PipWheelCache => "pip wheel cache",
        }
    }

    pub(crate) fn is_toolchain(self) -> bool {
        matches!(self, Self::Rust | Self::Node | Self::Python)
    }

    pub(crate) fn is_cache(self) -> bool {
        !self.is_toolchain()
    }

    pub(crate) fn cache_token(self) -> Option<&'static str> {
        match self {
            Self::CargoCache => Some("cargo-cache"),
            Self::CargoSource => Some("cargo-source"),
            Self::CargoIndex => Some("cargo-index"),
            Self::NpmCache => Some("npm-cache"),
            Self::PipHttpCache => Some("pip-http-cache"),
            Self::PipWheelCache => Some("pip-wheel-cache"),
            _ => None,
        }
    }

    fn cache_suffix(self) -> Option<&'static str> {
        match self {
            Self::CargoCache => Some(".cargo/registry/cache"),
            Self::CargoSource => Some(".cargo/registry/src"),
            Self::CargoIndex => Some(".cargo/registry/index"),
            Self::NpmCache => Some(".npm/_cacache"),
            Self::PipHttpCache => Some(".cache/pip/http-v2"),
            Self::PipWheelCache => Some(".cache/pip/wheels"),
            _ => None,
        }
    }

    pub(crate) fn cache_root(self) -> Option<&'static str> {
        match self {
            Self::CargoCache => Some("cargo/registry/cache"),
            Self::CargoSource => Some("cargo/registry/src"),
            Self::CargoIndex => Some("cargo/registry/index"),
            Self::NpmCache => Some(".npm/_cacache"),
            Self::PipHttpCache => Some(".cache/pip/http-v2"),
            Self::PipWheelCache => Some(".cache/pip/wheels"),
            _ => None,
        }
    }

    fn toolchain_token(self, grant: &RuntimeGrant) -> Result<String, ToolError> {
        let prefix = match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            _ => return Err(ToolError::Configuration),
        };
        let path = grant.path.to_str().ok_or(ToolError::Path)?;
        let mut identity = path.as_bytes().to_vec();
        identity.extend_from_slice(&grant.device.to_le_bytes());
        identity.extend_from_slice(&grant.inode.to_le_bytes());
        let hash = super::types::hex_digest(&identity);
        Ok(format!("{prefix}-{}", &hash[..12]))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeManager {
    Rustup,
    Nvm,
    Fnm,
    Volta,
    Asdf,
    Pyenv,
    HostCache,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeGrant {
    pub(crate) kind: RuntimeKind,
    pub(crate) manager: RuntimeManager,
    pub(crate) version: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl RuntimeGrant {
    pub(crate) fn token(&self) -> Result<String, ToolError> {
        self.kind
            .cache_token()
            .map(str::to_owned)
            .map_or_else(|| self.kind.toolchain_token(self), Ok)
    }

    pub(crate) fn validate_shape(&self) -> Result<(), ToolError> {
        if !self.path.is_absolute()
            || self.path.as_os_str().len() > 4096
            || self.path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || self.inode == 0
            || !self.matches_known_layout()
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }

    pub(crate) fn verify_host(&self) -> Result<(), ToolError> {
        self.validate_shape()?;
        self.verify_at(&self.path)
    }

    pub(crate) fn verify_at(&self, path: &Path) -> Result<(), ToolError> {
        let directory = fs::open_directory(path).map_err(|_| ToolError::ChangedInput)?;
        if fs::identity(&directory)? != (self.device, self.inode) {
            return Err(ToolError::ChangedInput);
        }
        if self.kind.is_toolchain() {
            match self.kind {
                RuntimeKind::Rust => {
                    if !executable(&path.join("bin/cargo")) || !executable(&path.join("bin/rustc"))
                    {
                        return Err(ToolError::ChangedInput);
                    }
                }
                RuntimeKind::Node => {
                    if !executable(&path.join("bin/node")) {
                        return Err(ToolError::ChangedInput);
                    }
                }
                RuntimeKind::Python => {
                    if !has_python_executable(path) {
                        return Err(ToolError::ChangedInput);
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn mounted_path(&self) -> Result<PathBuf, ToolError> {
        if self.kind.is_toolchain() {
            Ok(PathBuf::from("/runtime/toolchains").join(self.token()?))
        } else {
            let relative = self.kind.cache_root().ok_or(ToolError::Configuration)?;
            Ok(Path::new("/scratch").join(relative))
        }
    }

    fn matches_known_layout(&self) -> bool {
        if self.kind.is_cache() {
            return self.manager == RuntimeManager::HostCache
                && self.version.is_none()
                && self
                    .kind
                    .cache_suffix()
                    .is_some_and(|suffix| self.path.ends_with(suffix));
        }
        let Some(version) = self.version.as_deref() else {
            return false;
        };
        if version.len() > 128 || !is_versioned_installation(self.kind, version) {
            return false;
        }
        let suffix = match (self.kind, self.manager) {
            (RuntimeKind::Rust, RuntimeManager::Rustup) => {
                PathBuf::from(".rustup").join("toolchains").join(version)
            }
            (RuntimeKind::Node, RuntimeManager::Nvm) => PathBuf::from(".nvm")
                .join("versions")
                .join("node")
                .join(version),
            (RuntimeKind::Node, RuntimeManager::Fnm) => PathBuf::from(".local")
                .join("share")
                .join("fnm")
                .join("node-versions")
                .join(version)
                .join("installation"),
            (RuntimeKind::Node, RuntimeManager::Volta) => PathBuf::from(".volta")
                .join("tools")
                .join("image")
                .join("node")
                .join(version),
            (RuntimeKind::Node, RuntimeManager::Asdf) => PathBuf::from(".asdf")
                .join("installs")
                .join("nodejs")
                .join(version),
            (RuntimeKind::Python, RuntimeManager::Pyenv) => {
                PathBuf::from(".pyenv").join("versions").join(version)
            }
            _ => return false,
        };
        self.path.ends_with(suffix)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) token: String,
    pub(crate) label: String,
    pub(crate) grant: RuntimeGrant,
}

pub(crate) fn discover(home: &Path) -> Result<Vec<Candidate>, ToolError> {
    if !home.is_absolute() {
        return Err(ToolError::Path);
    }
    let mut candidates = Vec::new();
    for name in directory_names(&home.join(".rustup/toolchains"))? {
        let path = home.join(".rustup/toolchains").join(&name);
        if has_toolchain(&path, RuntimeKind::Rust) {
            push(
                &mut candidates,
                RuntimeKind::Rust,
                RuntimeManager::Rustup,
                name,
                path,
            )?;
        }
    }

    for (manager, root, nested) in [
        (RuntimeManager::Nvm, home.join(".nvm/versions/node"), false),
        (
            RuntimeManager::Fnm,
            home.join(".local/share/fnm/node-versions"),
            true,
        ),
        (
            RuntimeManager::Volta,
            home.join(".volta/tools/image/node"),
            false,
        ),
        (
            RuntimeManager::Asdf,
            home.join(".asdf/installs/nodejs"),
            false,
        ),
    ] {
        for version in directory_names(&root)? {
            let version_path = root.join(&version);
            let path = if nested {
                version_path.join("installation")
            } else {
                version_path
            };
            if has_toolchain(&path, RuntimeKind::Node) {
                push(&mut candidates, RuntimeKind::Node, manager, version, path)?;
            }
        }
    }

    for version in directory_names(&home.join(".pyenv/versions"))? {
        let path = home.join(".pyenv/versions").join(&version);
        if has_toolchain(&path, RuntimeKind::Python) {
            push(
                &mut candidates,
                RuntimeKind::Python,
                RuntimeManager::Pyenv,
                version,
                path,
            )?;
        }
    }

    for (kind, suffix) in [
        (RuntimeKind::CargoCache, ".cargo/registry/cache"),
        (RuntimeKind::CargoSource, ".cargo/registry/src"),
        (RuntimeKind::CargoIndex, ".cargo/registry/index"),
        (RuntimeKind::NpmCache, ".npm/_cacache"),
        (RuntimeKind::PipHttpCache, ".cache/pip/http-v2"),
        (RuntimeKind::PipWheelCache, ".cache/pip/wheels"),
    ] {
        let path = home.join(suffix);
        if let Some(grant) = admit_directory(kind, RuntimeManager::HostCache, None, path)? {
            push_candidate(&mut candidates, grant)?;
        }
    }
    candidates.sort_by(|left, right| left.token.cmp(&right.token));
    Ok(candidates)
}

pub(crate) fn validate_selection(grants: &[RuntimeGrant]) -> Result<(), ToolError> {
    if grants.len() > MAX_RUNTIME_GRANTS {
        return Err(ToolError::Limit);
    }
    let mut kinds = std::collections::BTreeSet::new();
    for grant in grants {
        grant.validate_shape()?;
        if !kinds.insert(grant.kind) {
            return Err(ToolError::Configuration);
        }
    }
    Ok(())
}

fn directory_names(path: &Path) -> Result<Vec<String>, ToolError> {
    let Ok(directory) = fs::open_directory(path) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for (index, entry) in directory
        .entries()
        .map_err(|_| ToolError::Path)?
        .enumerate()
    {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn has_toolchain(path: &Path, kind: RuntimeKind) -> bool {
    if fs::open_directory(path).is_err() {
        return false;
    }
    match kind {
        RuntimeKind::Rust => {
            executable(&path.join("bin/cargo")) && executable(&path.join("bin/rustc"))
        }
        RuntimeKind::Node => executable(&path.join("bin/node")),
        RuntimeKind::Python => has_python_executable(path),
        _ => false,
    }
}

fn executable(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    let Ok(file) = fs::open_absolute(path) else {
        return false;
    };
    executable_file(&file)
}

fn executable_file(file: &std::fs::File) -> bool {
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn has_python_executable(root: &Path) -> bool {
    let Ok(bin) = fs::open_directory(&root.join("bin")) else {
        return false;
    };
    for alias in ["python3", "python"] {
        if let Ok(target) = bin.read_link(alias) {
            let mut components = target.components();
            if let Some(std::path::Component::Normal(name)) = components.next()
                && components.next().is_none()
                && let Some(name) = name.to_str()
                && versioned_python_name(name)
                && let Ok(file) = fs::open_file(&bin, name)
                && executable_file(&file)
            {
                return true;
            }
        }
    }
    let Ok(entries) = bin.entries() else {
        return false;
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return false;
        }
        let Ok(entry) = entry else {
            return false;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if versioned_python_name(&name)
            && let Ok(file) = fs::open_file(&bin, &name)
            && executable_file(&file)
        {
            return true;
        }
    }
    false
}

fn versioned_python_name(name: &str) -> bool {
    let Some(version) = name.strip_prefix("python") else {
        return false;
    };
    let Some((major, minor)) = version.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit())
}

fn admit_directory(
    kind: RuntimeKind,
    manager: RuntimeManager,
    version: Option<String>,
    path: PathBuf,
) -> Result<Option<RuntimeGrant>, ToolError> {
    let Ok(directory) = fs::open_directory(&path) else {
        return Ok(None);
    };
    let (device, inode) = fs::identity(&directory)?;
    let grant = RuntimeGrant {
        kind,
        manager,
        version,
        path,
        device,
        inode,
    };
    grant.validate_shape()?;
    Ok(Some(grant))
}

fn push(
    candidates: &mut Vec<Candidate>,
    kind: RuntimeKind,
    manager: RuntimeManager,
    version: String,
    path: PathBuf,
) -> Result<(), ToolError> {
    if !is_versioned_installation(kind, &version) {
        return Ok(());
    }
    if let Some(grant) = admit_directory(kind, manager, Some(version), path)? {
        push_candidate(candidates, grant)?;
    }
    Ok(())
}

fn is_versioned_installation(kind: RuntimeKind, version: &str) -> bool {
    let version = if kind == RuntimeKind::Node {
        version.strip_prefix('v').unwrap_or(version)
    } else {
        version
    };
    let bytes = version.as_bytes();
    if bytes.len() > 128 {
        return false;
    }
    let mut position = 0;
    for component in 0..3 {
        let start = position;
        while bytes
            .get(position)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            position += 1;
        }
        if start == position
            || position - start > 1 && bytes[start] == b'0'
            || (component < 2 && bytes.get(position) != Some(&b'.'))
        {
            return false;
        }
        if component < 2 {
            position += 1;
        }
    }
    if position == bytes.len() {
        return true;
    }
    if !matches!(bytes[position], b'-' | b'+') {
        return false;
    }
    bytes[position + 1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && bytes.len() > position + 1
}

fn push_candidate(candidates: &mut Vec<Candidate>, grant: RuntimeGrant) -> Result<(), ToolError> {
    if candidates.len() >= MAX_CANDIDATES {
        return Err(ToolError::Limit);
    }
    let token = grant.token()?;
    let version = grant.version.as_deref().unwrap_or("selected cache");
    candidates.push(Candidate {
        label: format!(
            "{} · {} · {}",
            grant.kind.label(),
            version,
            grant.path.display()
        ),
        token,
        grant,
    });
    Ok(())
}

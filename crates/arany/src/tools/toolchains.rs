use super::{ToolError, fs};
use crate::validate::MAX_PATH_BYTES;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const MAX_RUNTIME_GRANTS: usize = 11;
const MAX_TOOLCHAIN_CANDIDATES: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 128;
#[cfg(target_os = "macos")]
const APPLE_CLT_USR: &str = "/Library/Developer/CommandLineTools/usr";
#[cfg(target_os = "macos")]
const APPLE_CLT_SDKS: &str = "/Library/Developer/CommandLineTools/SDKs";
// pip follows the platform cache directory.
#[cfg(target_os = "macos")]
const PIP_HOST_HTTP_CACHE: &str = "Library/Caches/pip/http-v2";
#[cfg(target_os = "macos")]
const PIP_HOST_WHEEL_CACHE: &str = "Library/Caches/pip/wheels";
#[cfg(not(target_os = "macos"))]
const PIP_HOST_HTTP_CACHE: &str = ".cache/pip/http-v2";
#[cfg(not(target_os = "macos"))]
const PIP_HOST_WHEEL_CACHE: &str = ".cache/pip/wheels";

#[cfg(target_os = "linux")]
pub(crate) fn sandbox_cache_root(cache_enabled: bool) -> &'static str {
    if cache_enabled { "/cache" } else { "/scratch" }
}

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
    AppleCompiler,
    AppleSdk,
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
            Self::AppleCompiler => "Command Line Tools compiler",
            Self::AppleSdk => "macOS SDK",
        }
    }

    pub(crate) fn is_toolchain(self) -> bool {
        matches!(
            self,
            Self::Rust | Self::Node | Self::Python | Self::AppleCompiler
        )
    }

    pub(crate) fn is_cache(self) -> bool {
        matches!(
            self,
            Self::CargoCache
                | Self::CargoSource
                | Self::CargoIndex
                | Self::NpmCache
                | Self::PipHttpCache
                | Self::PipWheelCache
        )
    }

    fn token_prefix(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::CargoCache => "cargo-cache",
            Self::CargoSource => "cargo-source",
            Self::CargoIndex => "cargo-index",
            Self::NpmCache => "npm-cache",
            Self::PipHttpCache => "pip-http-cache",
            Self::PipWheelCache => "pip-wheel-cache",
            Self::AppleCompiler => "apple-compiler",
            Self::AppleSdk => "apple-sdk",
        }
    }

    fn cache_layout(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::CargoCache => Some((".cargo/registry/cache", "cargo/registry/cache")),
            Self::CargoSource => Some((".cargo/registry/src", "cargo/registry/src")),
            Self::CargoIndex => Some((".cargo/registry/index", "cargo/registry/index")),
            Self::NpmCache => Some((".npm/_cacache", ".npm/_cacache")),
            Self::PipHttpCache => Some((PIP_HOST_HTTP_CACHE, ".cache/pip/http-v2")),
            Self::PipWheelCache => Some((PIP_HOST_WHEEL_CACHE, ".cache/pip/wheels")),
            _ => None,
        }
    }

    fn cache_suffix(self) -> Option<&'static str> {
        self.cache_layout().map(|(host, _)| host)
    }

    pub(crate) fn sandbox_cache_path(self) -> Option<&'static str> {
        self.cache_layout().map(|(_, sandbox)| sandbox)
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
    AppleCommandLineTools,
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
        let path = self.path.to_str().ok_or(ToolError::Path)?;
        let mut identity = path.as_bytes().to_vec();
        identity.extend_from_slice(&self.device.to_le_bytes());
        identity.extend_from_slice(&self.inode.to_le_bytes());
        let hash = crate::validate::hex_digest(&identity);
        Ok(format!("{}-{}", self.kind.token_prefix(), &hash[..12]))
    }

    pub(crate) fn profile_label(&self) -> String {
        format!(
            "{} · {} · {}",
            self.kind.label(),
            self.version.as_deref().unwrap_or(match self.kind {
                RuntimeKind::AppleCompiler => "selected CLT compiler",
                RuntimeKind::AppleSdk => "selected physical SDK",
                _ => "selected cache",
            }),
            self.path.display()
        )
    }

    pub(crate) fn validate_shape(&self) -> Result<(), ToolError> {
        if !self.path.is_absolute()
            || self.path.as_os_str().len() > MAX_PATH_BYTES
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

    pub(crate) fn reject_overlap(&self, protected: &[&Path]) -> Result<(), ToolError> {
        for path in protected {
            fs::reject_overlap(&self.path, path)?;
        }
        for runtime in super::guard::runtime_roots() {
            fs::reject_overlap(&self.path, Path::new(runtime))?;
        }
        Ok(())
    }

    pub(crate) fn verify_host(&self) -> Result<(), ToolError> {
        self.validate_shape()?;
        self.verify_at(&self.path)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn apple_compiler_executables(&self) -> Result<[PathBuf; 3], ToolError> {
        if self.kind != RuntimeKind::AppleCompiler {
            return Err(ToolError::Configuration);
        }
        self.validate_shape()?;
        Ok([
            self.path.join("bin/cc"),
            self.path.join("bin/clang"),
            self.path.join("bin/ld"),
        ])
    }

    pub(crate) fn verify_at(&self, path: &Path) -> Result<(), ToolError> {
        let directory = fs::open_directory(path).map_err(|_| ToolError::ChangedInput)?;
        if fs::identity(&directory)? != (self.device, self.inode) {
            return Err(ToolError::ChangedInput);
        }
        if !has_executables(self.kind, path) {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }

    // Mount Linux toolchains at their host paths so absolute shebangs resolve.
    #[cfg(target_os = "linux")]
    pub(crate) fn mounted_path(&self, cache_enabled: bool) -> Result<PathBuf, ToolError> {
        if self.kind.is_toolchain() {
            Ok(self.path.clone())
        } else {
            let relative = self
                .kind
                .sandbox_cache_path()
                .ok_or(ToolError::Configuration)?;
            Ok(Path::new(sandbox_cache_root(cache_enabled)).join(relative))
        }
    }

    fn matches_known_layout(&self) -> bool {
        if matches!(
            self.kind,
            RuntimeKind::AppleCompiler | RuntimeKind::AppleSdk
        ) {
            return apple_layout_matches(self);
        }
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
        if version.len() > 128 || installation_version(self.kind, version).is_none() {
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

    for kind in [
        RuntimeKind::CargoCache,
        RuntimeKind::CargoSource,
        RuntimeKind::CargoIndex,
        RuntimeKind::NpmCache,
        RuntimeKind::PipHttpCache,
        RuntimeKind::PipWheelCache,
    ] {
        let path = home.join(kind.cache_suffix().ok_or(ToolError::Configuration)?);
        if let Some(grant) = admit_directory(kind, RuntimeManager::HostCache, None, path)? {
            push_candidate(&mut candidates, grant)?;
        }
    }
    #[cfg(target_os = "macos")]
    discover_apple_runtimes(&mut candidates)?;
    candidates.sort_by(|left, right| {
        profile_order((&left.grant, &left.token), (&right.grant, &right.token))
    });
    Ok(candidates)
}

pub(super) fn default_profile_selection(
    candidates: &[Candidate],
    saved: &[RuntimeGrant],
) -> std::collections::BTreeSet<String> {
    use RuntimeKind::*;
    let mut selected = saved
        .iter()
        .filter_map(|grant| grant.token().ok().map(|token| (grant.kind, token)))
        .collect::<std::collections::BTreeMap<_, _>>();
    for kind in [
        Rust,
        Node,
        Python,
        CargoCache,
        CargoSource,
        CargoIndex,
        NpmCache,
        PipHttpCache,
        PipWheelCache,
        AppleCompiler,
        AppleSdk,
    ] {
        let enabled = match kind {
            Rust | Node | Python => true,
            CargoCache | CargoSource | CargoIndex | AppleCompiler | AppleSdk => {
                selected.contains_key(&Rust)
            }
            NpmCache => selected.contains_key(&Node),
            PipHttpCache | PipWheelCache => selected.contains_key(&Python),
        };
        if !enabled || selected.contains_key(&kind) {
            continue;
        }
        let newest = candidates
            .iter()
            .filter(|candidate| candidate.grant.kind == kind)
            .filter_map(|candidate| {
                let version = match kind {
                    Rust | Node | Python => {
                        installation_version(kind, candidate.grant.version.as_deref()?)?
                    }
                    AppleSdk => valid_apple_sdk_version(candidate.grant.version.as_deref()?)?,
                    CargoCache | CargoSource | CargoIndex | NpmCache | PipHttpCache
                    | PipWheelCache | AppleCompiler => (0, 0, 0),
                };
                Some((candidate, version))
            })
            .max_by(|(left, left_version), (right, right_version)| {
                left_version.cmp(right_version).then_with(|| {
                    profile_order((&right.grant, &right.token), (&left.grant, &left.token))
                })
            });
        if let Some((candidate, _)) = newest {
            selected.insert(kind, candidate.token.clone());
        }
    }
    selected.into_values().collect()
}

pub(super) fn profile_order(
    (left, left_token): (&RuntimeGrant, &str),
    (right, right_token): (&RuntimeGrant, &str),
) -> std::cmp::Ordering {
    left.kind
        .cmp(&right.kind)
        .then_with(|| right.version.cmp(&left.version))
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left_token.cmp(right_token))
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
    has_executables(kind, path)
}

fn has_executables(kind: RuntimeKind, path: &Path) -> bool {
    match kind {
        RuntimeKind::Rust => {
            executable(&path.join("bin/cargo")) && executable(&path.join("bin/rustc"))
        }
        RuntimeKind::Node => executable(&path.join("bin/node")),
        RuntimeKind::Python => has_python_executable(path),
        RuntimeKind::AppleCompiler => apple_compiler_valid(path),
        RuntimeKind::AppleSdk => apple_sdk_valid(path),
        RuntimeKind::CargoCache
        | RuntimeKind::CargoSource
        | RuntimeKind::CargoIndex
        | RuntimeKind::NpmCache
        | RuntimeKind::PipHttpCache
        | RuntimeKind::PipWheelCache => true,
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
    if installation_version(kind, &version).is_none() {
        return Ok(());
    }
    if let Some(grant) = admit_directory(kind, manager, Some(version), path)? {
        push_candidate(candidates, grant)?;
    }
    Ok(())
}

fn installation_version(kind: RuntimeKind, version: &str) -> Option<(u64, u64, u64)> {
    let version = if kind == RuntimeKind::Node {
        version.strip_prefix('v').unwrap_or(version)
    } else {
        version
    };
    let bytes = version.as_bytes();
    if bytes.len() > 128 {
        return None;
    }
    let mut position = 0;
    let mut numbers = [0; 3];
    for (component, number) in numbers.iter_mut().enumerate() {
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
            return None;
        }
        *number = version[start..position].parse().ok()?;
        if component < 2 {
            position += 1;
        }
    }
    if position == bytes.len() {
        return Some((numbers[0], numbers[1], numbers[2]));
    }
    if !matches!(bytes[position], b'-' | b'+') {
        return None;
    }
    (bytes[position + 1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && bytes.len() > position + 1)
        .then_some((numbers[0], numbers[1], numbers[2]))
}

fn apple_layout_matches(grant: &RuntimeGrant) -> bool {
    #[cfg(target_os = "macos")]
    {
        match grant.kind {
            RuntimeKind::AppleCompiler => {
                grant.manager == RuntimeManager::AppleCommandLineTools
                    && grant.version.is_none()
                    && grant.path == Path::new(APPLE_CLT_USR)
            }
            RuntimeKind::AppleSdk => {
                grant.manager == RuntimeManager::AppleCommandLineTools
                    && grant
                        .version
                        .as_deref()
                        .and_then(valid_apple_sdk_version)
                        .is_some()
                    && grant.path
                        == Path::new(APPLE_CLT_SDKS).join(format!(
                            "MacOSX{}.sdk",
                            grant.version.as_deref().unwrap_or_default()
                        ))
            }
            _ => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = grant;
        false
    }
}

fn valid_apple_sdk_version(version: &str) -> Option<(u64, u64, u64)> {
    if version.len() > 32 {
        return None;
    }
    let parts = version.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) || parts[0] == "0" {
        return None;
    }
    let mut numbers = [0; 3];
    for (part, number) in parts.iter().zip(&mut numbers) {
        let parsed = part.parse::<u16>().ok()?;
        if part.len() > 5 || parsed.to_string() != *part {
            return None;
        }
        *number = u64::from(parsed);
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

#[cfg(target_os = "macos")]
fn discover_apple_runtimes(candidates: &mut Vec<Candidate>) -> Result<(), ToolError> {
    let manager = RuntimeManager::AppleCommandLineTools;
    if let Some(grant) = admit_directory(
        RuntimeKind::AppleCompiler,
        manager,
        None,
        PathBuf::from(APPLE_CLT_USR),
    )? && grant.verify_host().is_ok()
    {
        push_candidate(candidates, grant)?;
    }

    let Ok(sdks) = fs::open_directory(Path::new(APPLE_CLT_SDKS)) else {
        return Ok(());
    };
    for (index, entry) in sdks.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(version) = name
            .strip_prefix("MacOSX")
            .and_then(|name| name.strip_suffix(".sdk"))
            .filter(|version| valid_apple_sdk_version(version).is_some())
        else {
            continue;
        };
        if !sdks
            .symlink_metadata(&name)
            .is_ok_and(|metadata| metadata.is_dir())
        {
            continue;
        }
        let path = Path::new(APPLE_CLT_SDKS).join(&name);
        let Some(grant) = admit_directory(
            RuntimeKind::AppleSdk,
            manager,
            Some(version.to_owned()),
            path,
        )?
        else {
            continue;
        };
        if grant.verify_host().is_ok() {
            push_candidate(candidates, grant)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn apple_compiler_valid(root: &Path) -> bool {
    use cap_std::fs::MetadataExt;

    let directories = [
        Path::new("/Library"),
        Path::new("/Library/Developer"),
        Path::new("/Library/Developer/CommandLineTools"),
        root,
        &root.join("bin"),
    ];
    if !directories.into_iter().all(system_directory) {
        return false;
    }
    let Ok(bin) = fs::open_directory(&root.join("bin")) else {
        return false;
    };
    if !bin
        .read_link("cc")
        .is_ok_and(|target| target.as_os_str() == "clang")
    {
        return false;
    }
    let Ok(link) = bin.symlink_metadata("cc") else {
        return false;
    };
    if link.uid() != 0 {
        return false;
    }
    let Ok(clang) = fs::open_file(&bin, "clang") else {
        return false;
    };
    system_executable(clang) && fs::open_file(&bin, "ld").is_ok_and(system_executable)
}

#[cfg(target_os = "macos")]
fn system_executable(file: std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;

    file.metadata().is_ok_and(|metadata| {
        fs::macos_system_file(
            metadata.is_file(),
            metadata.uid(),
            metadata.nlink(),
            metadata.mode(),
        ) && metadata.mode() & 0o111 != 0
    })
}

#[cfg(target_os = "macos")]
fn apple_sdk_valid(root: &Path) -> bool {
    let sdk_root = Path::new(APPLE_CLT_SDKS);
    let directories = [
        Path::new("/Library"),
        Path::new("/Library/Developer"),
        Path::new("/Library/Developer/CommandLineTools"),
        sdk_root,
        root,
        &root.join("usr"),
        &root.join("usr/lib"),
    ];
    directories.into_iter().all(system_directory)
        && system_file(root, "SDKSettings.plist")
        && system_directory(&root.join("usr/include"))
        && system_file(root, "usr/lib/libSystem.B.tbd")
}

#[cfg(target_os = "macos")]
fn system_directory(path: &Path) -> bool {
    use cap_std::fs::MetadataExt;

    let Ok(directory) = crate::private_fs::macos_directory(
        path,
        rustix::fs::OFlags::from_bits_retain(nix::libc::O_SEARCH as _),
    ) else {
        return false;
    };
    directory.dir_metadata().is_ok_and(|metadata| {
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    })
}

#[cfg(target_os = "macos")]
fn system_file(root: &Path, path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(directory) = fs::open_directory(root) else {
        return false;
    };
    let Ok(file) = fs::open_file(&directory, path) else {
        return false;
    };
    file.metadata().is_ok_and(|metadata| {
        metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    })
}

#[cfg(not(target_os = "macos"))]
fn apple_compiler_valid(_root: &Path) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn apple_sdk_valid(_root: &Path) -> bool {
    false
}

fn push_candidate(candidates: &mut Vec<Candidate>, grant: RuntimeGrant) -> Result<(), ToolError> {
    if candidates.len() >= MAX_TOOLCHAIN_CANDIDATES {
        return Err(ToolError::Limit);
    }
    let token = grant.token()?;
    candidates.push(Candidate {
        label: grant.profile_label(),
        token,
        grant,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sdk_versions_require_bounded_canonical_numeric_components() {
        for (label, expected) in [
            ("15.9", Some((15, 9, 0))),
            ("15.10.0", Some((15, 10, 0))),
            ("15.10.1", Some((15, 10, 1))),
            ("0.1", None),
            ("15.01", None),
            ("15.1.65536", None),
            ("15", None),
            ("15.1.0.0", None),
            ("15.+1", None),
        ] {
            assert_eq!(super::valid_apple_sdk_version(label), expected, "{label}");
        }
    }
}

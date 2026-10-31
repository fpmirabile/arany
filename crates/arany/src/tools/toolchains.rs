use super::{ToolError, fs};
use crate::validate::{MAX_PATH_BYTES, forbidden};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

const EXCLUSIVE_RUNTIME_GRANTS: usize = 11;
pub(crate) const MAX_USER_PROGRAM_GRANTS: usize = MAX_DISCOVERY_PATH_ENTRIES;
pub(crate) const MAX_MANAGER_SHIM_GRANTS: usize = 4;
pub(crate) const MAX_LINK_TARGET_GRANTS: usize = 16;
pub(crate) const MAX_LINK_SCAN_ENTRIES: usize = 16_384;
const MAX_EXTRA_READ_GRANTS: usize =
    super::guard_settings::MAX_SAVED_READ_GRANTS + super::guard_settings::MAX_RUN_READ_GRANTS;
pub(crate) const MAX_RUNTIME_GRANTS: usize = EXCLUSIVE_RUNTIME_GRANTS
    + MAX_USER_PROGRAM_GRANTS
    + MAX_MANAGER_SHIM_GRANTS
    + MAX_LINK_TARGET_GRANTS
    + MAX_EXTRA_READ_GRANTS;
// Homes are never system runtime, whoever owns them.
const HOME_ROOTS: [&str; 4] = ["/home", "/Users", "/root", "/private/var/root"];
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
    UserPrograms,
    ManagerShims,
    LinkTarget,
    ExtraRead,
}

struct ShimEntry {
    manager: RuntimeManager,
    directory: &'static str,
    homebrew: bool,
    root: &'static str,
    variable: &'static str,
}

const MANAGER_SHIMS: [ShimEntry; 5] = [
    ShimEntry {
        manager: RuntimeManager::Rustup,
        directory: ".cargo/bin",
        homebrew: false,
        root: ".rustup",
        variable: "RUSTUP_HOME",
    },
    ShimEntry {
        manager: RuntimeManager::Rustup,
        directory: "opt/rustup/bin",
        homebrew: true,
        root: ".rustup",
        variable: "RUSTUP_HOME",
    },
    ShimEntry {
        manager: RuntimeManager::Pyenv,
        directory: ".pyenv/shims",
        homebrew: false,
        root: ".pyenv",
        variable: "PYENV_ROOT",
    },
    ShimEntry {
        manager: RuntimeManager::Volta,
        directory: ".volta/bin",
        homebrew: false,
        root: ".volta",
        variable: "VOLTA_HOME",
    },
    ShimEntry {
        manager: RuntimeManager::Asdf,
        directory: ".asdf/shims",
        homebrew: false,
        root: ".asdf",
        variable: "ASDF_DATA_DIR",
    },
];

const MAX_DISCOVERY_PATH_BYTES: usize = super::command_environment::PATH_LIMIT_BYTES;
pub const MAX_DISCOVERY_PATH_ENTRIES: usize = 32;

const HOMEBREW_PREFIXES: [&str; 3] = ["/home/linuxbrew/.linuxbrew", "/opt/homebrew", "/usr/local"];
// Homebrew's opt/rustup/bin holds bash wrappers; SIP strips the page-size interposer from bash, so
// the grant pins the multicall directory they exec and sets the fallback settings they export.
const HOMEBREW_RUSTUP_MULTICALL: &str = "libexec/bin";
const HOMEBREW_RUSTUP_SETTINGS: &str = "etc/rustup/settings.toml";
const HOMEBREW_RUSTUP_SETTINGS_VARIABLE: &str = "RUSTUP_OVERRIDE_UNIX_FALLBACK_SETTINGS";

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
            Self::UserPrograms => "User programs",
            Self::ManagerShims => "Manager shims",
            Self::LinkTarget => "System link target",
            Self::ExtraRead => "Extra read",
        }
    }

    pub(crate) fn is_multi(self) -> bool {
        matches!(
            self,
            Self::UserPrograms | Self::ManagerShims | Self::LinkTarget | Self::ExtraRead
        )
    }

    // Link targets read and execute in place but never join the Guard PATH.
    pub(crate) fn on_path(self) -> bool {
        matches!(self, Self::UserPrograms | Self::ManagerShims)
    }

    // Both multi-select kinds share a rank so discovery (environment) order survives sorting.
    fn display_rank(self) -> Self {
        if self.is_multi() {
            Self::UserPrograms
        } else {
            self
        }
    }

    pub(crate) fn is_host_mounted(self) -> bool {
        self.is_toolchain() || self.is_multi()
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
            Self::UserPrograms => "user-programs",
            Self::ManagerShims => "manager-shims",
            Self::LinkTarget => "link-target",
            Self::ExtraRead => "extra-read",
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
    UserDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantRoot {
    pub(crate) path: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl GrantRoot {
    pub(crate) fn valid_shape(&self) -> bool {
        self.inode != 0
            && self.path.is_absolute()
            && self.path.as_os_str().len() <= MAX_PATH_BYTES
            && self
                .path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) root: Option<GrantRoot>,
    // One command's hidden protected names beneath this grant; never saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) masks: Vec<GrantMask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantMask {
    pub(crate) path: PathBuf,
    pub(crate) directory: bool,
}

impl RuntimeGrant {
    pub(crate) fn token(&self) -> Result<String, ToolError> {
        let path = self.path.to_str().ok_or(ToolError::Path)?;
        let mut identity = path.as_bytes().to_vec();
        identity.extend_from_slice(&self.device.to_le_bytes());
        identity.extend_from_slice(&self.inode.to_le_bytes());
        if let Some(root) = &self.root {
            identity.extend_from_slice(root.path.to_str().ok_or(ToolError::Path)?.as_bytes());
            identity.extend_from_slice(&root.device.to_le_bytes());
            identity.extend_from_slice(&root.inode.to_le_bytes());
        }
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
                RuntimeKind::UserPrograms
                | RuntimeKind::ManagerShims
                | RuntimeKind::LinkTarget
                | RuntimeKind::ExtraRead => "runs whatever the directory currently holds",
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
        for owned in std::iter::once(&self.path).chain(self.root.iter().map(|root| &root.path)) {
            for path in protected {
                fs::reject_overlap(owned, path)?;
            }
            if !self.in_system_root(owned) {
                for runtime in super::guard::runtime_roots() {
                    fs::reject_overlap(owned, Path::new(runtime))?;
                }
            }
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
        if let Some(root) = &self.root {
            let directory = fs::open_directory(&root.path).map_err(|_| ToolError::ChangedInput)?;
            if fs::identity(&directory)? != (root.device, root.inode) {
                return Err(ToolError::ChangedInput);
            }
        }
        Ok(())
    }

    // Mount Linux toolchains at their host paths so absolute shebangs resolve.
    #[cfg(target_os = "linux")]
    pub(crate) fn mounted_path(&self, cache_enabled: bool) -> Result<PathBuf, ToolError> {
        if self.kind.is_host_mounted() {
            Ok(self.path.clone())
        } else {
            let relative = self
                .kind
                .sandbox_cache_path()
                .ok_or(ToolError::Configuration)?;
            Ok(Path::new(sandbox_cache_root(cache_enabled)).join(relative))
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn mounts(&self, cache_enabled: bool) -> Result<Vec<(PathBuf, PathBuf)>, ToolError> {
        let mut mounts = Vec::new();
        if !self.in_system_root(&self.path) {
            mounts.push((self.path.clone(), self.mounted_path(cache_enabled)?));
        }
        if let Some(root) = self
            .root
            .as_ref()
            .filter(|root| !self.in_system_root(&root.path))
        {
            mounts.push((root.path.clone(), root.path.clone()));
        }
        Ok(mounts)
    }

    // A subtree of a fixed system mount such as /usr/local is already read-only and executable in the Guard.
    fn in_system_root(&self, path: &Path) -> bool {
        self.kind.is_multi()
            && super::guard::runtime_roots()
                .iter()
                .any(|runtime| path.starts_with(runtime))
    }

    pub(crate) fn manager_home(&self) -> Option<(&'static str, &Path)> {
        let root = self.root.as_ref()?;
        Some((self.shim_entry()?.variable, root.path.as_path()))
    }

    // Set only when the file is visible, as Homebrew's wrappers do.
    pub(crate) fn homebrew_rustup_settings(&self) -> Option<(&'static str, PathBuf)> {
        if self.kind != RuntimeKind::ManagerShims || self.manager != RuntimeManager::Rustup {
            return None;
        }
        let settings = homebrew_rustup_prefix(&self.path)?.join(HOMEBREW_RUSTUP_SETTINGS);
        settings
            .is_file()
            .then_some((HOMEBREW_RUSTUP_SETTINGS_VARIABLE, settings))
    }

    fn shim_entry(&self) -> Option<&'static ShimEntry> {
        MANAGER_SHIMS.iter().find(|entry| {
            entry.manager == self.manager
                && if entry.homebrew {
                    homebrew_rustup_prefix(&self.path).is_some()
                } else {
                    self.path.ends_with(entry.directory)
                }
        })
    }

    fn multi_layout_matches(&self) -> bool {
        if self.version.is_some() {
            return false;
        }
        match self.kind {
            RuntimeKind::LinkTarget | RuntimeKind::ExtraRead => {
                self.manager == RuntimeManager::UserDirectory && self.root.is_none()
            }
            RuntimeKind::UserPrograms => {
                self.manager == RuntimeManager::UserDirectory
                    && self
                        .root
                        .as_ref()
                        .is_none_or(|root| root.valid_shape() && self.path.starts_with(&root.path))
            }
            RuntimeKind::ManagerShims => {
                let (Some(entry), Some(root)) = (self.shim_entry(), self.root.as_ref()) else {
                    return false;
                };
                root.valid_shape()
                    && root.path.file_name().is_some_and(|name| name == entry.root)
                    && (entry.homebrew
                        || entry
                            .directory
                            .split('/')
                            .try_fold(self.path.as_path(), |path, _| path.parent())
                            .is_some_and(|base| root.path == base.join(entry.root)))
            }
            _ => false,
        }
    }

    fn matches_known_layout(&self) -> bool {
        if self.kind.is_multi() {
            return self.multi_layout_matches();
        }
        if self.root.is_some() {
            return false;
        }
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

    discover_node(home, &mut candidates)?;

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
        if let Some(grant) = admit_directory(kind, RuntimeManager::HostCache, None, path, None)? {
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

fn discover_node(home: &Path, candidates: &mut Vec<Candidate>) -> Result<(), ToolError> {
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
                push(candidates, RuntimeKind::Node, manager, version, path)?;
            }
        }
    }
    Ok(())
}

// Every PATH directory is granted per Run, so consent offers only manager shims and their roots.
pub(crate) fn discover_user_environment(
    home: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Result<Vec<Candidate>, ToolError> {
    let mut candidates = discover_user_programs(home, path, &HOMEBREW_PREFIXES)?;
    candidates.retain(|candidate| candidate.grant.kind == RuntimeKind::ManagerShims);
    Ok(candidates)
}

pub(crate) fn discover_user_programs(
    home: &Path,
    path: Option<&std::ffi::OsStr>,
    prefixes: &[&str],
) -> Result<Vec<Candidate>, ToolError> {
    if !home.is_absolute() {
        return Err(ToolError::Path);
    }
    let entries = scoped_path_entries(path)?;
    let shims = shim_candidates(home, prefixes)?;
    let mut candidates = Vec::new();
    for grant in order_by_environment(entries, shims, prefixes)? {
        push_candidate(&mut candidates, grant)?;
    }
    Ok(candidates)
}

fn scoped_path_entries(path: Option<&std::ffi::OsStr>) -> Result<Vec<PathBuf>, ToolError> {
    if path.is_some_and(|path| path.len() > MAX_DISCOVERY_PATH_BYTES) {
        return Err(ToolError::Limit);
    }
    let mut entries = distinct_path_entries(path);
    entries.truncate(MAX_DISCOVERY_PATH_ENTRIES);
    Ok(entries)
}

pub(crate) fn path_entries_truncated(path: &str) -> bool {
    distinct_path_entries(Some(std::ffi::OsStr::new(path))).len() > MAX_DISCOVERY_PATH_ENTRIES
}

fn distinct_path_entries(path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for entry in path.map(std::env::split_paths).into_iter().flatten() {
        if entry.is_absolute()
            && entry.components().all(|part| match part {
                Component::RootDir => true,
                Component::Normal(name) => name.to_str().is_some_and(|name| !forbidden(name)),
                _ => false,
            })
            && !platform_program_directory(&entry)
            && !entries.contains(&entry)
        {
            entries.push(entry);
        }
    }
    entries
}

// macOS platform binaries can carry entitlements, so only the per-program system catalogue admits them.
fn platform_program_directory(path: &Path) -> bool {
    super::executable::EXECUTABLES
        .platform_roots
        .iter()
        .any(|root| path.starts_with(root))
}

pub(crate) fn path_grants(
    path: &str,
    saved: &[RuntimeGrant],
    protected: &[&Path],
) -> Result<Vec<RuntimeGrant>, ToolError> {
    let mut grants: Vec<RuntimeGrant> = Vec::new();
    for entry in scoped_path_entries(Some(std::ffi::OsStr::new(path)))? {
        // An entry that cannot be pinned safely is left out rather than failing the Run.
        let Ok(Some(grant)) = user_program_grant(entry, &HOMEBREW_PREFIXES) else {
            continue;
        };
        if saved
            .iter()
            .chain(&grants)
            .any(|other| other.path == grant.path)
            || grant.reject_overlap(protected).is_err()
            || !grant.in_system_root(&grant.path) && super::guard::reserved_target(&grant.path)
        {
            continue;
        }
        grants.push(grant);
    }
    Ok(grants)
}

// A program in a granted directory that links outside every grant brings its target directory along
// when that directory is system-owned, so `/usr/local/bin/node -> /opt/node/bin/node` runs.
pub(crate) fn link_target_grants(
    grants: &[RuntimeGrant],
    protected: &[&Path],
) -> Vec<RuntimeGrant> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut targets: Vec<RuntimeGrant> = Vec::new();
    let mut budget = MAX_LINK_SCAN_ENTRIES;
    let directories = grants
        .iter()
        .filter(|grant| grant.kind.on_path())
        .map(|grant| grant.path.clone())
        .chain(["/usr/bin", "/bin"].map(PathBuf::from));
    for directory in directories {
        let Ok(listing) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut names = listing
            .take(budget)
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_symlink()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        budget = budget.saturating_sub(names.len());
        names.sort();
        for link in names {
            if targets.len() == MAX_LINK_TARGET_GRANTS {
                return targets;
            }
            let Some(target) = system_link_target(&link, home.as_deref()) else {
                continue;
            };
            let covered = super::guard::runtime_roots()
                .iter()
                .map(Path::new)
                .chain(grants.iter().chain(&targets).flat_map(|grant| {
                    std::iter::once(grant.path.as_path())
                        .chain(grant.root.as_ref().map(|root| root.path.as_path()))
                }))
                .any(|granted| target.starts_with(granted));
            if covered {
                continue;
            }
            if let Ok(Some(grant)) = admit_directory(
                RuntimeKind::LinkTarget,
                RuntimeManager::UserDirectory,
                None,
                target,
                None,
            ) && grant.reject_overlap(protected).is_ok()
                && !super::guard::reserved_target(&grant.path)
            {
                targets.push(grant);
            }
        }
    }
    targets
}

fn system_link_target(link: &Path, home: Option<&Path>) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let resolved = std::fs::canonicalize(link).ok()?;
    let metadata = std::fs::metadata(&resolved).ok()?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return None;
    }
    let directory = resolved.parent()?.to_owned();
    let names = directory
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let system_owned = directory.ancestors().all(|ancestor| {
        std::fs::symlink_metadata(ancestor)
            .is_ok_and(|metadata| metadata.uid() == 0 && metadata.mode() & 0o022 == 0)
    });
    (names.len() >= 2
        && !names.iter().any(|name| forbidden(name))
        && !platform_program_directory(&directory)
        && system_owned
        && !HOME_ROOTS
            .iter()
            .map(Path::new)
            .chain(home)
            .any(|home| directory.starts_with(home) || home.starts_with(&directory)))
    .then_some(directory)
}

fn shim_candidates(
    home: &Path,
    prefixes: &[&str],
) -> Result<Vec<(PathBuf, RuntimeGrant)>, ToolError> {
    let mut shims = Vec::new();
    for entry in &MANAGER_SHIMS {
        let bases = if entry.homebrew {
            prefixes.iter().map(PathBuf::from).collect()
        } else {
            vec![home.to_owned()]
        };
        for base in bases {
            if let Some(found) = shim_grant(entry, home, &base)? {
                shims.push(found);
                break;
            }
        }
    }
    Ok(shims)
}

// Environment order wins: the PATH-listed alternative of a manager replaces its siblings.
fn order_by_environment(
    entries: Vec<PathBuf>,
    mut pending: Vec<(PathBuf, RuntimeGrant)>,
    prefixes: &[&str],
) -> Result<Vec<RuntimeGrant>, ToolError> {
    let mut ordered: Vec<RuntimeGrant> = Vec::new();
    let mut excluded: Vec<PathBuf> = Vec::new();
    for entry in entries {
        if excluded.contains(&entry) {
            continue;
        }
        if let Some(index) = pending.iter().position(|(logical, _)| *logical == entry) {
            let grant = pending.remove(index).1;
            let (siblings, rest): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(|(_, other)| other.manager == grant.manager);
            excluded.extend(siblings.into_iter().map(|(logical, _)| logical));
            pending = rest;
            ordered.push(grant);
        } else if let Some(grant) = user_program_grant(entry, prefixes)?
            && !ordered.iter().any(|other| other.path == grant.path)
        {
            ordered.push(grant);
        }
    }
    for (_, grant) in pending {
        if !ordered.iter().any(|other| other.manager == grant.manager) {
            ordered.push(grant);
        }
    }
    Ok(ordered)
}

// A Homebrew prefix mounts whole for its libraries; opt/ aliases resolve inside it.
fn user_program_grant(
    entry: PathBuf,
    prefixes: &[&str],
) -> Result<Option<RuntimeGrant>, ToolError> {
    let Some(prefix) = prefixes.iter().find(|prefix| entry.starts_with(prefix)) else {
        return admit_directory(
            RuntimeKind::UserPrograms,
            RuntimeManager::UserDirectory,
            None,
            entry,
            None,
        );
    };
    let (Ok(physical), Ok(prefix)) = (std::fs::canonicalize(&entry), std::fs::canonicalize(prefix))
    else {
        return Ok(None);
    };
    let Ok(directory) = fs::open_directory(&prefix) else {
        return Ok(None);
    };
    if !physical.starts_with(&prefix) {
        return Ok(None);
    }
    let (device, inode) = fs::identity(&directory)?;
    admit_directory(
        RuntimeKind::UserPrograms,
        RuntimeManager::UserDirectory,
        None,
        physical,
        Some(GrantRoot {
            path: prefix,
            device,
            inode,
        }),
    )
}

fn shim_grant(
    entry: &ShimEntry,
    home: &Path,
    base: &Path,
) -> Result<Option<(PathBuf, RuntimeGrant)>, ToolError> {
    let logical = base.join(entry.directory);
    let mut path = logical.clone();
    if entry.homebrew {
        // Homebrew links opt/<formula> to the Cellar, so the physical directory is pinned.
        let multicall = logical.with_file_name(HOMEBREW_RUSTUP_MULTICALL);
        let (Ok(physical), Ok(prefix)) = (
            std::fs::canonicalize(multicall),
            std::fs::canonicalize(base),
        ) else {
            return Ok(None);
        };
        if homebrew_rustup_prefix(&physical) != Some(prefix.as_path()) {
            return Ok(None);
        }
        path = physical;
    }
    let root = home.join(entry.root);
    let Ok(directory) = fs::open_directory(&root) else {
        return Ok(None);
    };
    let (device, inode) = fs::identity(&directory)?;
    admit_directory(
        RuntimeKind::ManagerShims,
        entry.manager,
        None,
        path,
        Some(GrantRoot {
            path: root,
            device,
            inode,
        }),
    )
    .map(|grant| grant.map(|grant| (logical, grant)))
}

// The prefix of a Homebrew rustup multicall directory: Cellar/rustup/<version>/libexec/bin or its
// opt/rustup alias.
fn homebrew_rustup_prefix(path: &Path) -> Option<&Path> {
    let names = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    let depth = match names.as_slice() {
        [.., opt, formula, libexec, bin]
            if *opt == "opt" && *formula == "rustup" && *libexec == "libexec" && *bin == "bin" =>
        {
            4
        }
        [.., cellar, formula, _, libexec, bin]
            if *cellar == "Cellar"
                && *formula == "rustup"
                && *libexec == "libexec"
                && *bin == "bin" =>
        {
            5
        }
        _ => return None,
    };
    path.ancestors().nth(depth)
}

pub(super) fn default_profile_selection(
    candidates: &[Candidate],
    saved: &[RuntimeGrant],
) -> std::collections::BTreeSet<String> {
    use RuntimeKind::*;
    let mut selected = saved
        .iter()
        .filter(|grant| !grant.kind.is_multi())
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
            UserPrograms | ManagerShims | LinkTarget | ExtraRead => false,
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
                    | PipWheelCache | AppleCompiler | UserPrograms | ManagerShims | LinkTarget
                    | ExtraRead => (0, 0, 0),
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
    selected
        .into_values()
        .chain(
            saved
                .iter()
                .filter(|grant| grant.kind.is_multi())
                .filter_map(|grant| grant.token().ok()),
        )
        .collect()
}

pub(super) fn profile_order(
    (left, left_token): (&RuntimeGrant, &str),
    (right, right_token): (&RuntimeGrant, &str),
) -> std::cmp::Ordering {
    left.kind
        .display_rank()
        .cmp(&right.kind.display_rank())
        .then_with(|| right.version.cmp(&left.version))
        .then_with(|| {
            if left.kind.is_multi() {
                std::cmp::Ordering::Equal
            } else {
                left.path
                    .cmp(&right.path)
                    .then_with(|| left_token.cmp(right_token))
            }
        })
}

pub(crate) fn validate_selection(grants: &[RuntimeGrant]) -> Result<(), ToolError> {
    if grants.len() > MAX_RUNTIME_GRANTS {
        return Err(ToolError::Limit);
    }
    let mut kinds = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    let mut managers = Vec::new();
    let mut user_programs = 0;
    let mut link_targets = 0;
    let mut extra_reads = 0;
    for grant in grants {
        grant.validate_shape()?;
        if !grant.kind.is_multi() {
            if !kinds.insert(grant.kind) {
                return Err(ToolError::Configuration);
            }
            continue;
        }
        if !identities.insert((&grant.path, grant.device, grant.inode)) {
            return Err(ToolError::Configuration);
        }
        if grant.kind == RuntimeKind::UserPrograms {
            user_programs += 1;
            if user_programs > MAX_USER_PROGRAM_GRANTS {
                return Err(ToolError::Limit);
            }
        } else if grant.kind == RuntimeKind::LinkTarget {
            link_targets += 1;
            if link_targets > MAX_LINK_TARGET_GRANTS {
                return Err(ToolError::Limit);
            }
        } else if grant.kind == RuntimeKind::ExtraRead {
            extra_reads += 1;
            if extra_reads > MAX_EXTRA_READ_GRANTS {
                return Err(ToolError::Limit);
            }
        } else if managers.contains(&grant.manager) {
            return Err(ToolError::Configuration);
        } else {
            managers.push(grant.manager);
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
        | RuntimeKind::PipWheelCache
        | RuntimeKind::UserPrograms
        | RuntimeKind::ManagerShims
        | RuntimeKind::LinkTarget
        | RuntimeKind::ExtraRead => true,
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
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
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
    root: Option<GrantRoot>,
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
        root,
        masks: Vec::new(),
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
    if let Some(grant) = admit_directory(kind, manager, Some(version), path, None)? {
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
        None,
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
            None,
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
mod user_environment_tests {
    use super::*;

    fn directory(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        path.to_owned()
    }

    fn root_of(path: &Path) -> GrantRoot {
        let (device, inode) = fs::identity(&fs::open_directory(&directory(path)).unwrap()).unwrap();
        GrantRoot {
            path: path.to_owned(),
            device,
            inode,
        }
    }

    fn grant(
        kind: RuntimeKind,
        manager: RuntimeManager,
        path: &Path,
        root: Option<&Path>,
    ) -> RuntimeGrant {
        let (device, inode) = fs::identity(&fs::open_directory(path).unwrap()).unwrap();
        RuntimeGrant {
            kind,
            manager,
            version: None,
            path: path.to_owned(),
            device,
            inode,
            root: root.map(root_of),
            masks: Vec::new(),
        }
    }

    fn user_directory(path: &Path) -> RuntimeGrant {
        grant(
            RuntimeKind::UserPrograms,
            RuntimeManager::UserDirectory,
            path,
            None,
        )
    }

    fn paths(candidates: &[Candidate]) -> Vec<PathBuf> {
        candidates
            .iter()
            .map(|candidate| candidate.grant.path.clone())
            .collect()
    }

    #[test]
    fn homebrew_aliases_resolve_inside_the_prefix_and_rustup_follows_path_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        directory(&home.join(".rustup"));
        let cargo = directory(&home.join(".cargo/bin"));
        let local = directory(&home.join(".local/bin"));
        let volta = directory(&home.join(".volta/bin"));
        let outside = directory(&temp.path().join("outside/bin"));
        let prefix = std::fs::canonicalize(directory(&temp.path().join("brew"))).unwrap();
        directory(&prefix.join("opt"));
        directory(&prefix.join("Cellar/rustup/1.0/libexec/bin"));
        for formula in ["rustup", "tool"] {
            directory(&prefix.join(format!("Cellar/{formula}/1.0/bin")));
            std::os::unix::fs::symlink(
                format!("../Cellar/{formula}/1.0"),
                prefix.join(format!("opt/{formula}")),
            )
            .unwrap();
        }
        let escape = directory(&temp.path().join("elsewhere/bin"));
        std::os::unix::fs::symlink(&escape, prefix.join("opt/escape")).unwrap();
        let prefixes = [prefix.to_str().unwrap()];
        let search = |entries: &[PathBuf]| std::env::join_paths(entries).unwrap();

        let brew_first = search(&[
            prefix.join("opt/rustup/bin"),
            prefix.join("opt/tool/bin"),
            prefix.join("opt/escape"),
            local.clone(),
            cargo.clone(),
            local.clone(),
            outside.clone(),
            PathBuf::from("relative/bin"),
            home.join("missing"),
        ]);
        let found = discover_user_programs(&home, Some(&brew_first), &prefixes).unwrap();
        // Rustup's PATH entry selects the multicall directory behind Homebrew's bash wrappers.
        assert_eq!(
            paths(&found),
            [
                prefix.join("Cellar/rustup/1.0/libexec/bin"),
                prefix.join("Cellar/tool/1.0/bin"),
                local,
                outside,
                volta.clone()
            ]
        );
        assert_eq!(found[1].grant.root.as_ref().unwrap().path, prefix);
        let rustup = &found[0].grant;
        assert_eq!(rustup.manager_home().unwrap().1, home.join(".rustup"));
        assert_eq!(rustup.homebrew_rustup_settings(), None);
        let settings = prefix.join(HOMEBREW_RUSTUP_SETTINGS);
        directory(settings.parent().unwrap());
        std::fs::write(&settings, "version = \"12\"\n").unwrap();
        assert_eq!(
            rustup.homebrew_rustup_settings(),
            Some((HOMEBREW_RUSTUP_SETTINGS_VARIABLE, settings))
        );
        assert_eq!(found[1].grant.homebrew_rustup_settings(), None);
        let cargo_first = search(&[cargo.clone(), prefix.join("opt/rustup/bin")]);
        let found = discover_user_programs(&home, Some(&cargo_first), &prefixes).unwrap();
        assert_eq!(paths(&found), [cargo, volta]);
    }

    #[test]
    fn a_system_prefix_subtree_is_pinned_without_a_second_mount() {
        let bin = Path::new("/usr/local/bin");
        if !bin.is_dir() || std::fs::symlink_metadata(bin).unwrap().is_symlink() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = directory(&temp.path().join("project"));
        let path = std::env::join_paths([bin]).unwrap();
        let found = discover_user_programs(&home, Some(&path), &HOMEBREW_PREFIXES).unwrap();
        assert_eq!(paths(&found), [PathBuf::from("/usr/local/bin")]);
        let grant = &found[0].grant;
        grant.reject_overlap(&[workspace.as_path()]).unwrap();
        grant.verify_host().unwrap();
        validate_selection(std::slice::from_ref(grant)).unwrap();
        #[cfg(target_os = "linux")]
        assert!(grant.mounts(false).unwrap().is_empty());
        assert_eq!(
            super::super::guard::runtime_path(std::slice::from_ref(grant)).unwrap(),
            format!("/usr/local/bin:{}", super::super::guard::SYSTEM_PATH)
        );
        let hostile = user_directory(&workspace);
        assert!(hostile.reject_overlap(&[workspace.as_path()]).is_err());
    }

    #[test]
    fn discovery_bounds_path_bytes_and_scanned_directories_and_offers_every_survivor() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let many = (0..=MAX_DISCOVERY_PATH_ENTRIES)
            .map(|index| directory(&temp.path().join(format!("tools/{index}"))))
            .collect::<Vec<_>>();
        let path = std::env::join_paths(&many).unwrap();
        let candidates = discover_user_programs(&home, Some(&path), &[]).unwrap();
        assert_eq!(paths(&candidates), many[..MAX_DISCOVERY_PATH_ENTRIES]);
        assert!(path_entries_truncated(path.to_str().unwrap()));
        let repeated = std::env::join_paths(
            many[..MAX_DISCOVERY_PATH_ENTRIES]
                .iter()
                .chain(&many[..MAX_DISCOVERY_PATH_ENTRIES])
                .chain([&home.join(".ssh/bin")]),
        )
        .unwrap();
        assert!(!path_entries_truncated(repeated.to_str().unwrap()));
        assert_eq!(
            scoped_path_entries(Some(&repeated)).unwrap(),
            many[..MAX_DISCOVERY_PATH_ENTRIES],
            "duplicates count once and keep first-occurrence order; protected names never qualify"
        );

        let long = std::ffi::OsString::from("a".repeat(MAX_DISCOVERY_PATH_BYTES + 1));
        assert!(matches!(
            discover_user_programs(&home, Some(&long), &[]),
            Err(ToolError::Limit)
        ));
    }

    #[test]
    fn link_targets_grant_only_root_owned_directories_outside_every_grant() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let links = directory(&temp.path().join("links"));
        let tool = directory(&temp.path().join("tool")).join("run");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", links.join("covered")).unwrap();
        std::os::unix::fs::symlink(&tool, links.join("user-owned")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", links.join("not-executable")).unwrap();
        for link in ["user-owned", "not-executable"] {
            assert_eq!(system_link_target(&links.join(link), None), None, "{link}");
        }
        assert!(link_target_grants(&[user_directory(&links)], &[]).is_empty());
    }

    #[test]
    fn selection_allows_many_user_directories_but_keeps_toolchain_kinds_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let first = user_directory(&directory(&home.join("a")));
        let second = user_directory(&directory(&home.join("b")));
        validate_selection(&[first.clone(), second.clone()]).unwrap();
        assert!(validate_selection(&[first.clone(), first.clone()]).is_err());

        let many = (0..=MAX_USER_PROGRAM_GRANTS)
            .map(|index| user_directory(&directory(&home.join(format!("many/{index}")))))
            .collect::<Vec<_>>();
        assert!(matches!(validate_selection(&many), Err(ToolError::Limit)));

        let mut node = grant(
            RuntimeKind::Node,
            RuntimeManager::Nvm,
            &directory(&home.join(".nvm/versions/node/v22.3.0")),
            None,
        );
        node.version = Some("v22.3.0".into());
        validate_selection(&[node.clone(), first.clone()]).unwrap();
        assert!(validate_selection(&[node.clone(), node, first]).is_err());

        let rustup = grant(
            RuntimeKind::ManagerShims,
            RuntimeManager::Rustup,
            &directory(&home.join(".cargo/bin")),
            Some(&home.join(".rustup")),
        );
        let python = grant(
            RuntimeKind::ManagerShims,
            RuntimeManager::Pyenv,
            &directory(&home.join(".pyenv/shims")),
            Some(&home.join(".pyenv")),
        );
        validate_selection(&[rustup.clone(), python.clone()]).unwrap();

        let wrong_root = RuntimeGrant {
            root: Some(root_of(&home.join(".elsewhere"))),
            ..rustup.clone()
        };
        let no_root = RuntimeGrant {
            root: None,
            ..rustup.clone()
        };
        let wrong_manager = RuntimeGrant {
            manager: RuntimeManager::Volta,
            ..rustup.clone()
        };
        for malformed in [wrong_root, no_root, wrong_manager] {
            assert!(validate_selection(&[malformed]).is_err());
        }

        let first = grant(
            RuntimeKind::ManagerShims,
            RuntimeManager::Rustup,
            &directory(&home.join("brew/opt/rustup/libexec/bin")),
            Some(&home.join(".rustup")),
        );
        validate_selection(std::slice::from_ref(&first)).unwrap();
        assert!(validate_selection(&[rustup, first]).is_err());
    }

    #[test]
    fn user_directories_join_path_after_toolchains_and_set_only_manager_homes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let local = user_directory(&directory(&home.join(".local/bin")));
        let pyenv = grant(
            RuntimeKind::ManagerShims,
            RuntimeManager::Pyenv,
            &directory(&home.join(".pyenv/shims")),
            Some(&home.join(".pyenv")),
        );
        let python = home.join(".pyenv/versions/3.13.0");
        directory(&python.join("bin"));
        let toolchain = RuntimeGrant {
            version: Some("3.13.0".into()),
            ..grant(RuntimeKind::Python, RuntimeManager::Pyenv, &python, None)
        };
        let expected = format!(
            "{}/bin:{}:{}:{}",
            python.display(),
            local.path.display(),
            pyenv.path.display(),
            super::super::guard::SYSTEM_PATH
        );
        assert_eq!(
            super::super::guard::runtime_path(&[local, pyenv, toolchain]).unwrap(),
            expected
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{EXCLUSIVE_RUNTIME_GRANTS, RuntimeKind};

    #[test]
    fn exclusive_runtime_grants_count_the_non_multi_kinds() {
        let kinds = [
            RuntimeKind::Rust,
            RuntimeKind::Node,
            RuntimeKind::Python,
            RuntimeKind::CargoCache,
            RuntimeKind::CargoSource,
            RuntimeKind::CargoIndex,
            RuntimeKind::NpmCache,
            RuntimeKind::PipHttpCache,
            RuntimeKind::PipWheelCache,
            RuntimeKind::AppleCompiler,
            RuntimeKind::AppleSdk,
            RuntimeKind::UserPrograms,
            RuntimeKind::ManagerShims,
            RuntimeKind::LinkTarget,
            RuntimeKind::ExtraRead,
        ];
        assert_eq!(
            kinds.iter().filter(|kind| !kind.is_multi()).count(),
            EXCLUSIVE_RUNTIME_GRANTS
        );
    }

    #[test]
    fn user_program_discovery_offers_only_platform_enforced_grants() {
        use super::*;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let local = home.join(".local/bin");
        for path in [&local, &home.join(".cargo/bin"), &home.join(".rustup")] {
            std::fs::create_dir_all(path).unwrap();
        }
        let path = std::env::join_paths([&local, &home.join(".cargo/bin")]).unwrap();
        let candidates = discover_user_environment(home, Some(&path)).unwrap();
        let grants = candidates
            .iter()
            .map(|candidate| candidate.grant.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            grants.iter().map(|grant| grant.kind).collect::<Vec<_>>(),
            [RuntimeKind::ManagerShims]
        );
        validate_selection(&grants).unwrap();
        for grant in grants {
            grant.verify_host().unwrap();
        }
    }

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

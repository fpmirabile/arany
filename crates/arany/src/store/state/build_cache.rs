use super::{StateRoot, StoreError, nonblocking};
use crate::validate::MAX_PATH_BYTES;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, MetadataExt, OpenOptions};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Component, Path};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod directory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use directory::{CacheChange, CacheGuardRecovery};
#[cfg(test)]
mod tests;

pub(super) const DIRECTORY: &str = "build-cache";
pub(super) const GUARD_PREFIX: &str = "guard-";
pub(super) const TREE_PREFIX: &str = "directory-";
pub(super) const DIRTY_PREFIX: &str = "unavailable-";
const IDENTITY_LOCK_PREFIX: &str = "identity-";
const IDENTITY_FILE_KINDS: [(&str, &str); 4] = [
    (GUARD_PREFIX, ".unit"),
    (TREE_PREFIX, ".tree"),
    (DIRTY_PREFIX, ".pending"),
    (IDENTITY_LOCK_PREFIX, ".lock"),
];
// Records of the superseded archive format: never read; a cache holding one is regenerable, so it is
// discarded at its next lease or evicted. First match wins: "transaction-next-" precedes "transaction-".
const SUPERSEDED_KINDS: [(&str, &str); 8] = [
    ("cache-", ".arc"),
    ("previous-", ".arc"),
    ("stage-", ".arc"),
    ("candidate-", ".arc"),
    ("generation-", ".generation"),
    ("reserve-", ".reserve"),
    ("transaction-next-", ".pending"),
    ("transaction-", ".pending"),
];
const NAMESPACE_LOCK: &str = "namespace.lock";
const SUPERSEDED_LOCK: &str = "aggregate.lock";
pub(super) const MAX_IDENTITIES: usize = 128;
const MEBIBYTE_BYTES: u64 = 1024 * 1024;
pub(crate) const DEFAULT_VIEW_BYTES: u64 = 64 * MEBIBYTE_BYTES;
const MAX_DIRECTORY_ENTRIES: usize =
    MAX_IDENTITIES * (IDENTITY_FILE_KINDS.len() + SUPERSEDED_KINDS.len()) + 2;
const MAX_PATH_DEPTH: usize = 32;
pub(crate) const CACHE_ROOTS: &[&str] = &[
    "target",
    "cargo/registry/cache",
    "cargo/registry/src",
    "cargo/registry/index",
    "cargo/git/db",
    "cargo/git/checkouts",
    ".npm/_cacache",
    ".cache/pip/http-v2",
    ".cache/pip/wheels",
];

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildCacheIdentity {
    pub(crate) path: String,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl BuildCacheIdentity {
    pub(crate) fn new(path: &Path, device: u64, inode: u64) -> Result<Self, StoreError> {
        if !valid_identity_path(path) || inode == 0 {
            return Err(StoreError::InvalidStateDirectory);
        }
        let path = path
            .to_str()
            .ok_or(StoreError::InvalidStateDirectory)?
            .to_owned();
        Ok(Self {
            path,
            device,
            inode,
        })
    }

    pub(crate) fn token(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.path.as_bytes());
        digest.update(self.device.to_le_bytes());
        digest.update(self.inode.to_le_bytes());
        hex(&digest.finalize())
    }
}

fn valid_identity_path(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    if !path.is_absolute() || value.len() > MAX_PATH_BYTES || path == Path::new("/") {
        return false;
    }
    let mut normalized = String::with_capacity(value.len());
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push('/'),
            Component::Normal(name) => {
                if !normalized.ends_with('/') {
                    normalized.push('/');
                }
                let Some(name) = name.to_str() else {
                    return false;
                };
                normalized.push_str(name);
            }
            _ => return false,
        }
    }
    normalized == value
}

pub(crate) struct BuildCacheLease {
    dir: Dir,
    _identity_lock: FlockGuard,
    token: String,
    candidate_limit: u64,
    uncertain: bool,
}

impl BuildCacheLease {
    pub(crate) fn candidate_limit(&self) -> u64 {
        self.candidate_limit
    }

    pub(crate) fn publication_uncertain(&self) -> bool {
        self.uncertain
    }
}

impl StateRoot {
    fn admit_identity(&self, identity: &BuildCacheIdentity) -> Result<(), StoreError> {
        if !valid_identity_path(Path::new(&identity.path)) || identity.inode == 0 {
            return Err(StoreError::InvalidStateDirectory);
        }
        let workspace = crate::private_fs::open_workspace_directory(Path::new(&identity.path))
            .map_err(StoreError::from)?;
        self.ensure_outside_workspace(&workspace)?;
        let metadata = workspace.dir_metadata()?;
        if metadata.dev() != identity.device || metadata.ino() != identity.inode {
            return Err(StoreError::InvalidStateDirectory);
        }
        Ok(())
    }

    #[cfg(any(test, all(target_os = "linux", feature = "test-support")))]
    pub(crate) fn begin_build_cache(
        &self,
        identity: BuildCacheIdentity,
    ) -> Result<BuildCacheLease, StoreError> {
        self.begin_build_cache_with_limit(identity, DEFAULT_VIEW_BYTES)
    }

    // The committed cache plus the StateRoot volume's free space above `reserve`, in whole MiB.
    // Admission only measures; eviction runs off the async runtime when the cache session starts.
    pub(crate) fn build_cache_headroom(
        &self,
        identity: &BuildCacheIdentity,
        reserve: u64,
    ) -> Result<u64, StoreError> {
        self.admit_identity(identity)?;
        let dir = cache_directory(self, true)?;
        let token = identity.token();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        directory::directory_available(&dir, &token)?;
        match open_regular(&dir, &identity_lock_name(&token), 0) {
            Ok(lock) => touch(&lock)?,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let free = volume_free_bytes(&dir)?;
        let headroom = own_tree_bytes(&dir, &token)?.saturating_add(free.saturating_sub(reserve));
        Ok(headroom / MEBIBYTE_BYTES * MEBIBYTE_BYTES)
    }

    // Evicts whole unleased Workspace caches, least recently used first, until the StateRoot volume
    // has `free_bytes` free or nothing evictable remains; returns the volume's free bytes.
    pub(crate) fn evict_build_caches(&self, free_bytes: u64) -> Result<u64, StoreError> {
        match cache_directory(self, false) {
            Ok(dir) => evict_until_free(&dir, free_bytes),
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => {
                volume_free_bytes(&self.dir)
            }
            Err(error) => Err(error),
        }
    }

    pub fn build_cache_bytes(&self) -> Result<u64, StoreError> {
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut total = 0u64;
        for (count, entry) in dir.entries()?.enumerate() {
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            let bytes = if let Some(token) = suffix(&name, TREE_PREFIX, ".tree") {
                validate_token(token)?;
                own_tree_bytes(&dir, token)?
            } else if superseded_record(&name) {
                dir.symlink_metadata(&name)?.len()
            } else {
                0
            };
            total = total.saturating_add(bytes);
        }
        Ok(total)
    }

    pub(crate) fn begin_build_cache_with_limit(
        &self,
        identity: BuildCacheIdentity,
        candidate_limit: u64,
    ) -> Result<BuildCacheLease, StoreError> {
        self.admit_identity(&identity)?;
        let dir = cache_directory(self, true)?;
        let token = identity.token();
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        if !contains_cache_identity(&dir, &token)? && identity_count(&dir)? >= MAX_IDENTITIES {
            evict_lru(&dir, |dir| Ok(identity_count(dir)? < MAX_IDENTITIES))?;
            if identity_count(&dir)? >= MAX_IDENTITIES {
                return Err(StoreError::StorageFull);
            }
        }
        let (identity_file, _) = open_lock_file(&dir, &identity_lock_name(&token))?;
        let identity_lock = FlockGuard::acquire(identity_file, true)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            directory::guard::admit(&dir, &token)?;
            directory::directory_available(&dir, &token)?;
        }
        if has_superseded(&dir, &token)? {
            remove_cache_files(&dir, &token)?;
        }
        touch(&identity_lock.0)?;
        Ok(BuildCacheLease {
            dir,
            _identity_lock: identity_lock,
            token,
            candidate_limit,
            uncertain: false,
        })
    }

    pub(crate) fn clear_build_cache(&self, identity: BuildCacheIdentity) -> Result<(), StoreError> {
        self.admit_identity(&identity)?;
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let token = identity.token();
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let (identity_file, _) = open_lock_file(&dir, &identity_lock_name(&token))?;
        let _identity_lock = FlockGuard::acquire(identity_file, true)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        directory::guard::admit(&dir, &token)?;
        remove_identity(&dir, &token)
    }

    pub fn clear_workspace_build_cache(&self, workspace: &Path) -> Result<(), StoreError> {
        let opened =
            crate::private_fs::open_workspace_directory(workspace).map_err(StoreError::from)?;
        self.ensure_outside_workspace(&opened)?;
        let metadata = opened.dir_metadata()?;
        let identity = BuildCacheIdentity::new(workspace, metadata.dev(), metadata.ino())?;
        self.clear_build_cache(identity)
    }

    pub fn clear_all_build_caches(&self) -> Result<(), StoreError> {
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let mut locks = Vec::new();
        for (count, entry) in dir.entries()?.enumerate() {
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            if let Some(token) = suffix(&name, IDENTITY_LOCK_PREFIX, ".lock") {
                locks.push(token.to_owned());
            }
        }
        let mut held = Vec::new();
        for token in &locks {
            validate_token(token)?;
            let file = open_regular(&dir, &identity_lock_name(token), 0)?;
            held.push(FlockGuard::acquire(file, true)?);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            directory::guard::admit(&dir, token)?;
        }
        for (count, entry) in dir.entries()?.enumerate() {
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            if name.starts_with(IDENTITY_LOCK_PREFIX) {
                continue;
            }
            if name.ends_with(".tree") && name.starts_with(TREE_PREFIX) {
                dir.open_dir_nofollow(&name)?;
                dir.remove_dir_all(&name)?;
            } else if name == SUPERSEDED_LOCK || record_token(&name).is_some() {
                dir.remove_file(&name)?;
            }
        }
        sync_directory(&dir)?;
        for token in &locks {
            dir.remove_file(identity_lock_name(token))?;
        }
        drop(held);
        Ok(())
    }
}

fn evict_until_free(dir: &Dir, target: u64) -> Result<u64, StoreError> {
    if volume_free_bytes(dir)? < target {
        match lock_file(dir, NAMESPACE_LOCK, true) {
            Ok(_namespace) => evict_lru(dir, |dir| Ok(volume_free_bytes(dir)? >= target))?,
            Err(StoreError::BuildCacheBusy) => {}
            Err(error) => return Err(error),
        }
    }
    volume_free_bytes(dir)
}

// The caller holds the namespace lock. A lease or Guard record keeps a cache.
fn evict_lru(
    dir: &Dir,
    mut enough: impl FnMut(&Dir) -> Result<bool, StoreError>,
) -> Result<(), StoreError> {
    let mut caches = Vec::new();
    for (count, entry) in dir.entries()?.enumerate() {
        if count >= MAX_DIRECTORY_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        if let Some(token) = suffix(&name, IDENTITY_LOCK_PREFIX, ".lock") {
            validate_token(token)?;
            caches.push((entry.metadata()?.modified()?, token.to_owned()));
        }
    }
    caches.sort();
    for (_, token) in caches {
        if enough(dir)? {
            break;
        }
        let lock = match open_regular(dir, &identity_lock_name(&token), 0) {
            Ok(lock) => lock,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let _held = match FlockGuard::acquire(lock, true) {
            Ok(held) => held,
            Err(StoreError::BuildCacheBusy) => continue,
            Err(error) => return Err(error),
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if directory::guard::active(dir, &token)? {
            continue;
        }
        remove_identity(dir, &token)?;
    }
    Ok(())
}

// The caller holds the identity lock; the lock file goes last.
fn remove_identity(dir: &Dir, token: &str) -> Result<(), StoreError> {
    remove_cache_files(dir, token)?;
    remove_if_present(dir, &identity_lock_name(token))
}

// Everything but the held identity lock, so no second lessee can lock a recreated file meanwhile.
fn remove_cache_files(dir: &Dir, token: &str) -> Result<(), StoreError> {
    let lock = identity_lock_name(token);
    for name in identity_file_names(token).filter(|name| *name != lock) {
        if name.starts_with(TREE_PREFIX) {
            if file_exists(dir, &name)? {
                dir.open_dir_nofollow(&name)?;
                dir.remove_dir_all(&name)?;
            }
        } else {
            remove_if_present(dir, &name)?;
        }
    }
    sync_directory(dir)
}

fn volume_free_bytes(dir: &Dir) -> Result<u64, StoreError> {
    let capacity = rustix::fs::fstatvfs(dir).map_err(|error| StoreError::Io(error.into()))?;
    Ok(capacity.f_bavail.saturating_mul(capacity.f_frsize))
}

// The identity lock's modification time is the cache's last use.
fn touch(file: &File) -> Result<(), StoreError> {
    let now = rustix::fs::Timespec {
        tv_sec: 0,
        tv_nsec: rustix::fs::UTIME_NOW,
    };
    rustix::fs::futimens(
        file,
        &rustix::fs::Timestamps {
            last_access: now,
            last_modification: now,
        },
    )
    .map_err(|error| StoreError::Io(error.into()))
}

fn check_source_file(metadata: &cap_std::fs::Metadata) -> Result<(), StoreError> {
    if !metadata.is_file() || metadata.mode() & 0o6000 != 0 {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

// Visits every entry under the allowlisted roots, holding one directory handle per level.
fn walk(
    tree: &Dir,
    visit: &mut impl FnMut(&cap_std::fs::Metadata) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for root in CACHE_ROOTS {
        let mut parts = root.split('/').collect::<Vec<_>>();
        let leaf = parts.pop().ok_or(StoreError::InvalidBuildCache)?;
        let parent = match open_relative_dir(tree, &parts) {
            Ok(parent) => parent,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let metadata = match parent.symlink_metadata(leaf) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        walk_entry(&parent, leaf, root, &metadata, visit)?;
    }
    Ok(())
}

fn walk_entry(
    parent: &Dir,
    name: &str,
    path: &str,
    metadata: &cap_std::fs::Metadata,
    visit: &mut impl FnMut(&cap_std::fs::Metadata) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    validate_path(path)?;
    if metadata.is_dir() {
        let dir = parent.open_dir_nofollow(name)?;
        for entry in dir.entries()? {
            let entry = entry?;
            let child = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            validate_component(&child)?;
            let child_metadata = dir.symlink_metadata(&child)?;
            walk_entry(
                &dir,
                &child,
                &format!("{path}/{child}"),
                &child_metadata,
                visit,
            )?;
        }
    } else {
        check_source_file(metadata)?;
    }
    visit(metadata)
}

fn create_parent(root: &Dir, path: &str) -> Result<(Dir, String), StoreError> {
    validate_path(path)?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let leaf = parts.pop().ok_or(StoreError::InvalidBuildCache)?.to_owned();
    let mut dir = root.try_clone()?;
    let mut builder = cap_std::fs::DirBuilder::new();
    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
    for part in parts {
        match dir.open_dir_nofollow(part) {
            Ok(opened) => {
                dir = opened;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match dir.create_dir_with(part, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        dir = dir.open_dir_nofollow(part)?;
    }
    Ok((dir, leaf))
}

fn open_relative_dir(root: &Dir, components: &[&str]) -> Result<Dir, StoreError> {
    let mut dir = root.try_clone()?;
    for component in components {
        validate_component(component)?;
        dir = dir.open_dir_nofollow(component)?;
    }
    Ok(dir)
}

fn metadata_relative(root: &Dir, path: &str) -> Result<cap_std::fs::Metadata, StoreError> {
    validate_path(path)?;
    let mut components = path.split('/').collect::<Vec<_>>();
    let leaf = components.pop().ok_or(StoreError::InvalidBuildCache)?;
    open_relative_dir(root, &components)?
        .symlink_metadata(leaf)
        .map_err(StoreError::from)
}

fn open_nofollow(root: &Dir, path: &str) -> Result<File, StoreError> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let leaf = parts.pop().ok_or(StoreError::InvalidBuildCache)?;
    let parent = open_relative_dir(root, &parts)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let file = parent.open_with(leaf, &options)?;
    if !file.metadata()?.is_file() {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(file)
}

fn cache_directory(state: &StateRoot, create: bool) -> Result<Dir, StoreError> {
    if create {
        let mut builder = cap_std::fs::DirBuilder::new();
        cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match state.dir.create_dir_with(DIRECTORY, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let dir = state.dir.open_dir_nofollow(DIRECTORY)?;
    crate::private_fs::check_acl(&dir, true)?;
    let metadata = dir.dir_metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(dir)
}

fn open_lock_file(dir: &Dir, name: &str) -> Result<(File, bool), StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    match dir.open_with(name, &options) {
        Ok(file) => {
            check_private(&file, 0)?;
            Ok((file, false))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut create = OpenOptions::new();
            create
                .read(true)
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            cap_std::fs::OpenOptionsExt::mode(&mut create, 0o600);
            nonblocking(&mut create);
            match dir.open_with(name, &create) {
                Ok(file) => {
                    check_private(&file, 0)?;
                    Ok((file, true))
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let mut retry = OpenOptions::new();
                    retry.read(true).write(true).follow(FollowSymlinks::No);
                    nonblocking(&mut retry);
                    let file = dir.open_with(name, &retry)?;
                    check_private(&file, 0)?;
                    Ok((file, false))
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn lock_file(dir: &Dir, name: &str, try_only: bool) -> Result<FlockGuard, StoreError> {
    let (file, _) = open_lock_file(dir, name)?;
    FlockGuard::acquire(file, try_only)
}

// An explicit unlock releases the lock even while a forked child still holds a copy of the descriptor.
struct FlockGuard(File);

impl FlockGuard {
    fn acquire(file: File, try_only: bool) -> Result<Self, StoreError> {
        lock_exclusive(&file, try_only)?;
        Ok(Self(file))
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

fn lock_exclusive(file: &File, try_only: bool) -> Result<(), StoreError> {
    let operation = if try_only {
        rustix::fs::FlockOperation::NonBlockingLockExclusive
    } else {
        rustix::fs::FlockOperation::LockExclusive
    };
    match rustix::fs::flock(file, operation) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(StoreError::BuildCacheBusy),
        Err(error) => Err(StoreError::Io(io::Error::from(error))),
    }
}

fn create_private_file(dir: &Dir, name: &str) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    cap_std::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.create_new(true);
    nonblocking(&mut options);
    let file = dir.open_with(name, &options)?;
    check_private(&file, 0)?;
    Ok(file)
}

fn open_regular(dir: &Dir, name: &str, max: u64) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let file = dir.open_with(name, &options)?;
    check_private(&file, max)?;
    Ok(file)
}

fn check_private(file: &File, max: u64) -> Result<(), StoreError> {
    crate::private_fs::check_acl(file, true)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() > max
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn own_tree_bytes(dir: &Dir, token: &str) -> Result<u64, StoreError> {
    let name = directory::tree_name(token);
    if !file_exists(dir, &name)? {
        return Ok(0);
    }
    directory::measure(&dir.open_dir_nofollow(&name)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn own_tree_bytes(_dir: &Dir, _token: &str) -> Result<u64, StoreError> {
    Ok(0)
}

fn superseded_record(name: &str) -> bool {
    SUPERSEDED_KINDS
        .iter()
        .any(|(prefix, ending)| suffix(name, prefix, ending).is_some())
}

fn has_superseded(dir: &Dir, token: &str) -> Result<bool, StoreError> {
    for (prefix, ending) in SUPERSEDED_KINDS {
        if file_exists(dir, &format!("{prefix}{token}{ending}"))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn record_token(name: &str) -> Option<&str> {
    IDENTITY_FILE_KINDS
        .iter()
        .chain(SUPERSEDED_KINDS.iter())
        .find_map(|(prefix, ending)| suffix(name, prefix, ending))
}

pub(super) fn inventory_record(name: &str) -> Option<(Option<&str>, bool)> {
    if matches!(name, NAMESPACE_LOCK | SUPERSEDED_LOCK) {
        return Some((None, false));
    }
    let token = record_token(name)?;
    validate_token(token).ok()?;
    let retained = !(name.starts_with(TREE_PREFIX) || name.starts_with(IDENTITY_LOCK_PREFIX));
    Some((Some(token), retained))
}

fn identity_count(dir: &Dir) -> Result<usize, StoreError> {
    let mut identities = std::collections::BTreeSet::new();
    for (count, entry) in dir.entries()?.enumerate() {
        if count >= MAX_DIRECTORY_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        if let Some(token) = record_token(&name) {
            validate_token(token)?;
            identities.insert(token.to_owned());
        }
    }
    Ok(identities.len())
}

fn identity_file_names(token: &str) -> impl Iterator<Item = String> + '_ {
    IDENTITY_FILE_KINDS
        .into_iter()
        .chain(SUPERSEDED_KINDS)
        .map(move |(prefix, ending)| format!("{prefix}{token}{ending}"))
}

fn contains_cache_identity(dir: &Dir, token: &str) -> Result<bool, StoreError> {
    for name in identity_file_names(token) {
        if file_exists(dir, &name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_if_present(dir: &Dir, name: &str) -> Result<(), StoreError> {
    match dir.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn sync_directory(dir: &Dir) -> Result<(), StoreError> {
    dir.open_with(".", OpenOptions::new().read(true))?
        .sync_all()?;
    Ok(())
}

fn file_exists(dir: &Dir, name: &str) -> Result<bool, StoreError> {
    match dir.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn validate_component(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 255
        || value.contains('/')
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidBuildCache);
    }
    Ok(())
}

pub(crate) fn validate_path(path: &str) -> Result<(), StoreError> {
    if path.len() > MAX_PATH_BYTES || path.split('/').count() > MAX_PATH_DEPTH || !allowed(path) {
        return Err(StoreError::InvalidBuildCache);
    }
    for component in path.split('/') {
        validate_component(component)?;
    }
    Ok(())
}

fn allowed(path: &str) -> bool {
    CACHE_ROOTS.iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

fn is_excluded(path: &str, exclusions: &[String]) -> bool {
    exclusions.iter().any(|prefix| {
        path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

fn identity_lock_name(token: &str) -> String {
    format!("{IDENTITY_LOCK_PREFIX}{token}.lock")
}

fn suffix<'a>(name: &'a str, prefix: &str, ending: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)?.strip_suffix(ending)
}

fn validate_token(token: &str) -> Result<(), StoreError> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn io_kind(error: &StoreError) -> Option<io::ErrorKind> {
    match error {
        StoreError::Io(error) => Some(error.kind()),
        _ => None,
    }
}

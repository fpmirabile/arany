use super::{StateRoot, StoreError, nonblocking};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, File, MetadataExt, OpenOptions};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path};

const DIRECTORY: &str = "build-cache";
const ARCHIVE_PREFIX: &str = "cache-";
const PREVIOUS_PREFIX: &str = "previous-";
const RESERVATION_PREFIX: &str = "reserve-";
const STAGING_PREFIX: &str = "stage-";
const CANDIDATE_PREFIX: &str = "candidate-";
const IDENTITY_LOCK_PREFIX: &str = "identity-";
const TRANSACTION_PREFIX: &str = "transaction-";
const TRANSACTION_NEXT_PREFIX: &str = "transaction-next-";
const NAMESPACE_LOCK: &str = "namespace.lock";
const AGGREGATE_LOCK: &str = "aggregate.lock";
const MAX_IDENTITIES: usize = 128;
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const RESERVED_TRANSIENT_BYTES: u64 = MAX_ARCHIVE_BYTES * 2;
pub(crate) const AGGREGATE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const IDENTITY_METADATA_BYTES: u64 = 1024;
const MAX_DIRECTORY_ENTRIES: usize = MAX_IDENTITIES * 8 + 2;
const MAX_ARCHIVE_ENTRIES: usize = 16_384;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PATH_DEPTH: usize = 32;
const MAX_PATH_INDEX_BYTES: usize = 8 * 1024 * 1024;
const MAGIC: &[u8; 12] = b"ARANYCACHE\0\x01";
const CACHE_ROOTS: &[&str] = &[
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
        let path = path.canonicalize()?;
        if !valid_identity_path(&path) {
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

    fn token(&self) -> String {
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
    _identity_lock: File,
    identity: BuildCacheIdentity,
    token: String,
    archive: Option<File>,
    defer_recovery: bool,
}

impl BuildCacheLease {
    pub(crate) fn archive(&self) -> Result<Option<(File, u64)>, StoreError> {
        let Some(file) = self.archive.as_ref() else {
            return Ok(None);
        };
        let opened = open_regular(&self.dir, &archive_name(&self.token), MAX_ARCHIVE_BYTES)?;
        let opened_metadata = opened.metadata()?;
        let file_metadata = file.metadata()?;
        if opened_metadata.dev() != file_metadata.dev()
            || opened_metadata.ino() != file_metadata.ino()
        {
            return Err(StoreError::StateNotPrivate);
        }
        Ok(Some((opened, file.metadata()?.len())))
    }

    pub(crate) fn publish<R: Read>(
        &mut self,
        candidate: &mut R,
        exclusions: &[String],
    ) -> Result<(), StoreError> {
        let _aggregate = lock_file(&self.dir, AGGREGATE_LOCK, true)?;
        let old = self.archive.as_ref().map(|_| archive_name(&self.token));
        let staged = stage_name(&self.token);
        let mut stage = create_private_file(&self.dir, &staged)?;
        let mut candidate = ArchiveReader::new(candidate, &self.identity)?;
        let mut old_file = old
            .as_ref()
            .map(|name| open_regular(&self.dir, name, MAX_ARCHIVE_BYTES))
            .transpose()?;
        if let (Some(expected), Some(current)) = (self.archive.as_ref(), old_file.as_ref()) {
            let expected = expected.metadata()?;
            let current = current.metadata()?;
            if expected.dev() != current.dev() || expected.ino() != current.ino() {
                return Err(StoreError::StateNotPrivate);
            }
        }
        let mut previous = match old_file.as_mut() {
            Some(file) => Some(ArchiveReader::new(file, &self.identity)?),
            None => None,
        };
        write_header(&mut stage, &self.identity)?;
        let mut candidate_entry = candidate.next()?;
        let mut previous_entry = previous
            .as_mut()
            .map(ArchiveReader::next)
            .transpose()?
            .flatten();
        let mut bytes = header_size(&self.identity)?;
        let mut entries = 0usize;
        while candidate_entry.is_some() || previous_entry.is_some() {
            let ordering = match (&candidate_entry, &previous_entry) {
                (Some(candidate), Some(previous)) => compare_paths(&candidate.path, &previous.path),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => break,
            };
            match ordering {
                std::cmp::Ordering::Less => {
                    let mut entry = candidate_entry
                        .take()
                        .ok_or(StoreError::InvalidBuildCache)?;
                    if !is_excluded(&entry.path, exclusions) {
                        write_entry(&mut stage, &mut entry, &mut bytes, &mut entries)?;
                    } else {
                        entry.skip()?;
                    }
                    candidate_entry = candidate.next()?;
                }
                std::cmp::Ordering::Greater => {
                    let mut entry = previous_entry.take().ok_or(StoreError::InvalidBuildCache)?;
                    if is_excluded(&entry.path, exclusions) {
                        write_entry(&mut stage, &mut entry, &mut bytes, &mut entries)?;
                    } else {
                        entry.skip()?;
                    }
                    previous_entry = previous
                        .as_mut()
                        .map(ArchiveReader::next)
                        .transpose()?
                        .flatten();
                }
                std::cmp::Ordering::Equal => {
                    let mut current = candidate_entry
                        .take()
                        .ok_or(StoreError::InvalidBuildCache)?;
                    let mut old = previous_entry.take().ok_or(StoreError::InvalidBuildCache)?;
                    if is_excluded(&old.path, exclusions) {
                        write_entry(&mut stage, &mut old, &mut bytes, &mut entries)?;
                        current.skip()?;
                    } else if !is_excluded(&current.path, exclusions) {
                        write_entry(&mut stage, &mut current, &mut bytes, &mut entries)?;
                        old.skip()?;
                    } else {
                        old.skip()?;
                        current.skip()?;
                    }
                    candidate_entry = candidate.next()?;
                    previous_entry = previous
                        .as_mut()
                        .map(ArchiveReader::next)
                        .transpose()?
                        .flatten();
                }
            }
        }
        candidate.finish()?;
        if let Some(mut previous) = previous {
            previous.finish()?;
        }
        write_terminator(&mut stage)?;
        if bytes.checked_add(12).ok_or(StoreError::StorageFull)? > MAX_ARCHIVE_BYTES {
            self.dir.remove_file(&staged)?;
            return Err(StoreError::StorageFull);
        }
        let usage = inventory(&self.dir)?;
        if usage > AGGREGATE_BYTES {
            self.dir.remove_file(&staged)?;
            return Err(StoreError::StorageFull);
        }
        let name = archive_name(&self.token);
        stage.sync_all()?;
        let committed = stage.try_clone()?;
        let transaction = if old.is_some() {
            CacheTransaction::RollbackPrevious
        } else {
            CacheTransaction::RollbackEmpty
        };
        write_transaction(&self.dir, &self.token, transaction)?;
        let mut commit_marker_attempted = false;
        let operation = (|| {
            if old.is_some() {
                self.dir
                    .rename(&name, &self.dir, previous_name(&self.token))?;
                sync_directory(&self.dir)?;
            }
            self.dir.rename(&staged, &self.dir, &name)?;
            sync_directory(&self.dir)?;
            set_reservation(&self.dir, &self.token, MAX_ARCHIVE_BYTES)?;
            sync_directory(&self.dir)?;
            commit_marker_attempted = true;
            write_transaction(
                &self.dir,
                &self.token,
                if old.is_some() {
                    CacheTransaction::CommittedPrevious
                } else {
                    CacheTransaction::CommittedEmpty
                },
            )?;
            self.archive = Some(committed);
            Ok::<(), StoreError>(())
        })();
        if let Err(error) = operation {
            if commit_marker_attempted {
                self.defer_recovery = true;
                return Err(error);
            }
            let recovery = recover_transaction(&self.dir, &self.token);
            return match recovery {
                Ok(()) => Err(error),
                Err(_) => Err(StoreError::StateNotPrivate),
            };
        }
        let _ = finish_committed(&self.dir, &self.token);
        Ok(())
    }

    pub(crate) fn candidate_writer(&self) -> Result<File, StoreError> {
        let _aggregate = lock_file(&self.dir, AGGREGATE_LOCK, true)?;
        create_private_file(&self.dir, &candidate_name(&self.token))
    }

    pub(crate) fn open_candidate(&self) -> Result<File, StoreError> {
        open_regular(&self.dir, &candidate_name(&self.token), MAX_ARCHIVE_BYTES)
    }
}

impl Drop for BuildCacheLease {
    fn drop(&mut self) {
        if self.defer_recovery {
            return;
        }
        if let Ok(_aggregate) = lock_file(&self.dir, AGGREGATE_LOCK, true) {
            let _ = recover_transaction(&self.dir, &self.token);
        }
    }
}

impl StateRoot {
    pub(crate) fn begin_build_cache(
        &self,
        identity: BuildCacheIdentity,
    ) -> Result<BuildCacheLease, StoreError> {
        let workspace = crate::private_fs::open_directory(Path::new(&identity.path), false)
            .map_err(StoreError::from)?;
        self.ensure_outside_workspace(&workspace.dir)?;
        let workspace_metadata = workspace.dir.dir_metadata()?;
        if workspace_metadata.dev() != identity.device || workspace_metadata.ino() != identity.inode
        {
            return Err(StoreError::InvalidStateDirectory);
        }
        let dir = cache_directory(self, true)?;
        let token = identity.token();
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let lock_name = identity_lock_name(&token);
        let known_identity = contains_cache_identity(&dir, &token)?;
        if identity_count(&dir)? >= MAX_IDENTITIES && !known_identity {
            return Err(StoreError::StorageFull);
        }
        let (identity_lock, created_lock) = open_lock_file(&dir, &lock_name)?;
        lock_exclusive(&identity_lock, true)?;
        let mut reservation_created = false;
        let archive_result = (|| {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, true)?;
            recover_transaction(&dir, &token)?;
            remove_stale_candidates(&dir)?;
            let archive_name = archive_name(&token);
            let archive = match open_regular(&dir, &archive_name, MAX_ARCHIVE_BYTES) {
                Ok(file) => {
                    let mut validation_file = file.try_clone()?;
                    ArchiveReader::new(&mut validation_file, &identity)?.validate_all()?;
                    Some(file)
                }
                Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => None,
                Err(error) => return Err(error),
            };
            let projected = inventory(&dir)?
                .checked_add(RESERVED_TRANSIENT_BYTES)
                .ok_or(StoreError::StorageFull)?;
            if projected > AGGREGATE_BYTES {
                return Err(StoreError::StorageFull);
            }
            let mut reservation = create_private_file(&dir, &reservation_name(&token))?;
            reservation_created = true;
            write_reservation(&mut reservation, &token, RESERVED_TRANSIENT_BYTES)?;
            reservation.sync_all()?;
            dir.open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            Ok(archive)
        })();
        let archive = match archive_result {
            Ok(archive) => archive,
            Err(error) => {
                let cleanup_safe = if reservation_created {
                    remove_if_present(&dir, &reservation_name(&token)).is_ok()
                        && sync_directory(&dir).is_ok()
                } else {
                    true
                };
                drop(identity_lock);
                if created_lock && cleanup_safe && !has_cache_files(&dir, &token)? {
                    remove_if_present(&dir, &lock_name)?;
                    let _ = sync_directory(&dir);
                }
                return Err(error);
            }
        };
        Ok(BuildCacheLease {
            dir,
            _identity_lock: identity_lock,
            identity,
            token,
            archive,
            defer_recovery: false,
        })
    }

    pub(crate) fn clear_build_cache(&self, identity: BuildCacheIdentity) -> Result<(), StoreError> {
        let workspace = crate::private_fs::open_directory(Path::new(&identity.path), false)
            .map_err(StoreError::from)?;
        self.ensure_outside_workspace(&workspace.dir)?;
        let metadata = workspace.dir.dir_metadata()?;
        if metadata.dev() != identity.device || metadata.ino() != identity.inode {
            return Err(StoreError::InvalidStateDirectory);
        }
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let token = identity.token();
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let lock_name = identity_lock_name(&token);
        let (identity_lock, _) = open_lock_file(&dir, &lock_name)?;
        lock_exclusive(&identity_lock, true)?;
        let _aggregate = lock_file(&dir, AGGREGATE_LOCK, true)?;
        remove_if_present(&dir, &archive_name(&token))?;
        remove_if_present(&dir, &previous_name(&token))?;
        remove_if_present(&dir, &stage_name(&token))?;
        remove_if_present(&dir, &candidate_name(&token))?;
        remove_if_present(&dir, &reservation_name(&token))?;
        remove_if_present(&dir, &transaction_name(&token))?;
        remove_if_present(&dir, &transaction_next_name(&token))?;
        dir.open_with(".", OpenOptions::new().read(true))?
            .sync_all()?;
        remove_if_present(&dir, &lock_name)?;
        Ok(())
    }

    pub fn clear_workspace_build_cache(&self, workspace: &Path) -> Result<(), StoreError> {
        let canonical = workspace.canonicalize()?;
        let opened =
            crate::private_fs::open_directory(&canonical, false).map_err(StoreError::from)?;
        self.ensure_outside_workspace(&opened.dir)?;
        let metadata = opened.dir.dir_metadata()?;
        let identity = BuildCacheIdentity::new(&opened.path, metadata.dev(), metadata.ino())?;
        self.clear_build_cache(identity)
    }

    pub fn clear_all_build_caches(&self) -> Result<(), StoreError> {
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let mut names = Vec::new();
        let mut scanned = 0usize;
        for entry in dir.entries()? {
            scanned += 1;
            if scanned > MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            if name.starts_with(IDENTITY_LOCK_PREFIX) && name.ends_with(".lock") {
                names.push(name);
            }
        }
        let mut held = Vec::new();
        for name in &names {
            let file = open_regular(&dir, name, 0)?;
            lock_exclusive(&file, true)?;
            held.push(file);
        }
        let _aggregate = lock_file(&dir, AGGREGATE_LOCK, true)?;
        let mut scanned = 0usize;
        for entry in dir.entries()? {
            scanned += 1;
            if scanned > MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            if name.starts_with(ARCHIVE_PREFIX)
                || name.starts_with(PREVIOUS_PREFIX)
                || name.starts_with(RESERVATION_PREFIX)
                || name.starts_with(STAGING_PREFIX)
                || name.starts_with(CANDIDATE_PREFIX)
                || name.starts_with(IDENTITY_LOCK_PREFIX)
                || name.starts_with(TRANSACTION_PREFIX)
                || name.starts_with(TRANSACTION_NEXT_PREFIX)
            {
                dir.remove_file(&name)?;
            }
        }
        dir.open_with(".", OpenOptions::new().read(true))?
            .sync_all()?;
        drop(held);
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn restore_archive<R: Read>(
    input: &mut R,
    destination: &Dir,
    identity: &BuildCacheIdentity,
    exclusions: &[String],
) -> Result<(), StoreError> {
    let mut archive = ArchiveReader::new(input, identity)?;
    while let Some(mut entry) = archive.next()? {
        if is_excluded(&entry.path, exclusions) {
            entry.skip()?;
            continue;
        }
        if entry.directory {
            let (parent, leaf) = create_parent(destination, &entry.path)?;
            let mut builder = DirBuilder::new();
            cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match parent.create_dir_with(&leaf, &builder) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    parent.open_dir_nofollow(&leaf)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            let (parent, leaf) = create_parent(destination, &entry.path)?;
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            cap_std::fs::OpenOptionsExt::mode(
                &mut options,
                if entry.executable { 0o700 } else { 0o600 },
            );
            let mut file = parent.open_with(&leaf, &options)?;
            entry.copy_to(&mut file)?;
        }
    }
    archive.finish()
}

pub(crate) async fn restore_archive_async<R: tokio::io::AsyncRead + Unpin>(
    input: &mut R,
    destination: &Dir,
    identity: &BuildCacheIdentity,
    exclusions: &[String],
) -> Result<(), StoreError> {
    use tokio::io::AsyncReadExt;

    let mut magic = [0; MAGIC.len()];
    input.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        return Err(StoreError::InvalidBuildCache);
    }
    let path_len = read_u16_async(input).await? as usize;
    let device = read_u64_async(input).await?;
    let inode = read_u64_async(input).await?;
    if path_len == 0 || path_len > MAX_PATH_BYTES {
        return Err(StoreError::InvalidBuildCache);
    }
    let mut path = vec![0; path_len];
    input.read_exact(&mut path).await?;
    if path != identity.path.as_bytes() || device != identity.device || inode != identity.inode {
        return Err(StoreError::InvalidBuildCache);
    }
    let mut last: Option<String> = None;
    let mut total = header_size(identity)?;
    let mut entries = 0usize;
    loop {
        let path_len = read_u16_async(input).await? as usize;
        let kind = read_u8_async(input).await?;
        let flags = read_u8_async(input).await?;
        let size = read_u64_async(input).await?;
        if path_len == 0 {
            if kind != 0 || flags != 0 || size != 0 {
                return Err(StoreError::InvalidBuildCache);
            }
            total = total.checked_add(12).ok_or(StoreError::StorageFull)?;
            if total > MAX_ARCHIVE_BYTES {
                return Err(StoreError::StorageFull);
            }
            let mut extra = [0; 1];
            if input.read(&mut extra).await? != 0 {
                return Err(StoreError::InvalidBuildCache);
            }
            return Ok(());
        }
        if path_len > MAX_PATH_BYTES {
            return Err(StoreError::InvalidBuildCache);
        }
        let mut path_bytes = vec![0; path_len];
        input.read_exact(&mut path_bytes).await?;
        let path = String::from_utf8(path_bytes).map_err(|_| StoreError::InvalidBuildCache)?;
        validate_path(&path)?;
        let directory = match kind {
            0 if size == 0 => true,
            1 => false,
            _ => return Err(StoreError::InvalidBuildCache),
        };
        if flags & !1 != 0 || directory && flags != 0 || !allowed(&path) {
            return Err(StoreError::InvalidBuildCache);
        }
        if last
            .as_ref()
            .is_some_and(|last| compare_paths(last, &path) != std::cmp::Ordering::Less)
        {
            return Err(StoreError::InvalidBuildCache);
        }
        last = Some(path.clone());
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        total = total
            .checked_add(
                entry_header_size(&path)?
                    .checked_add(size)
                    .ok_or(StoreError::StorageFull)?,
            )
            .ok_or(StoreError::StorageFull)?;
        if total.checked_add(12).ok_or(StoreError::StorageFull)? > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
        if is_excluded(&path, exclusions) {
            discard_async(input, size).await?;
            continue;
        }
        if directory {
            let (parent, leaf) = create_parent(destination, &path)?;
            let mut builder = DirBuilder::new();
            cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match parent.create_dir_with(&leaf, &builder) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    parent.open_dir_nofollow(&leaf)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            let (parent, leaf) = create_parent(destination, &path)?;
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            cap_std::fs::OpenOptionsExt::mode(&mut options, if flags == 1 { 0o700 } else { 0o600 });
            let mut file = parent.open_with(&leaf, &options)?;
            copy_async_to_file(input, &mut file, size).await?;
        }
    }
}

async fn copy_async_to_file<R: tokio::io::AsyncRead + Unpin>(
    input: &mut R,
    output: &mut File,
    mut remaining: u64,
) -> Result<(), StoreError> {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0; 32 * 1024];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        input.read_exact(&mut buffer[..count]).await?;
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    Ok(())
}

async fn discard_async<R: tokio::io::AsyncRead + Unpin>(
    input: &mut R,
    mut remaining: u64,
) -> Result<(), StoreError> {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0; 32 * 1024];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        input.read_exact(&mut buffer[..count]).await?;
        remaining -= count as u64;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn capture_archive<W: Write>(
    source: &Dir,
    identity: &BuildCacheIdentity,
    exclusions: &[String],
    writer: &mut W,
) -> Result<u64, StoreError> {
    let expected_size = archive_size(source, identity, exclusions)?;
    write_header(writer, identity)?;
    let mut total = header_size(identity)?;
    let paths = cache_paths(source, exclusions)?;
    for path in paths.iter() {
        let metadata = metadata_relative(source, path)?;
        let directory = metadata.is_dir();
        let executable = !directory && metadata.mode() & 0o111 != 0;
        if !directory
            && (!metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o6000 != 0)
        {
            return Err(StoreError::StateNotPrivate);
        }
        let size = if directory { 0 } else { metadata.len() };
        if !directory && size > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
        let entry_size = entry_header_size(path)?
            .checked_add(size)
            .ok_or(StoreError::StorageFull)?;
        total = total
            .checked_add(entry_size)
            .ok_or(StoreError::StorageFull)?;
        if total.checked_add(12).ok_or(StoreError::StorageFull)? > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
        write_entry_header(writer, path, directory, executable, size)?;
        if !directory {
            let mut file = open_nofollow(source, path)?;
            {
                let mut limited = (&mut file).take(size);
                if io::copy(&mut limited, writer)? != size {
                    return Err(StoreError::InvalidBuildCache);
                }
            }
            let mut extra = [0; 1];
            if file.read(&mut extra)? != 0 {
                return Err(StoreError::InvalidBuildCache);
            }
        }
    }
    write_terminator(writer)?;
    total = total.checked_add(12).ok_or(StoreError::StorageFull)?;
    if total != expected_size {
        return Err(StoreError::InvalidBuildCache);
    }
    Ok(total)
}

pub(crate) fn archive_size(
    source: &Dir,
    identity: &BuildCacheIdentity,
    exclusions: &[String],
) -> Result<u64, StoreError> {
    let paths = cache_paths(source, exclusions)?;
    let mut total = header_size(identity)?;
    for path in paths {
        let metadata = metadata_relative(source, &path)?;
        let directory = metadata.is_dir();
        if !directory
            && (!metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o6000 != 0)
        {
            return Err(StoreError::StateNotPrivate);
        }
        let size = if directory { 0 } else { metadata.len() };
        total = total
            .checked_add(
                entry_header_size(&path)?
                    .checked_add(size)
                    .ok_or(StoreError::StorageFull)?,
            )
            .ok_or(StoreError::StorageFull)?;
        if total.checked_add(12).ok_or(StoreError::StorageFull)? > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
    }
    total.checked_add(12).ok_or(StoreError::StorageFull)
}

pub(crate) async fn capture_archive_async<W: tokio::io::AsyncWrite + Unpin>(
    source: &Dir,
    identity: &BuildCacheIdentity,
    exclusions: &[String],
    writer: &mut W,
) -> Result<u64, StoreError> {
    use tokio::io::AsyncWriteExt;

    let expected_size = archive_size(source, identity, exclusions)?;
    let mut header = Vec::with_capacity(MAGIC.len() + 2 + 16 + identity.path.len());
    write_header(&mut header, identity)?;
    writer.write_all(&header).await?;
    let paths = cache_paths(source, exclusions)?;
    let mut total = header_size(identity)?;
    let mut buffer = [0; 32 * 1024];
    for path in paths.iter() {
        let metadata = metadata_relative(source, path)?;
        let directory = metadata.is_dir();
        let executable = !directory && metadata.mode() & 0o111 != 0;
        if !directory
            && (!metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o6000 != 0)
        {
            return Err(StoreError::StateNotPrivate);
        }
        let size = if directory { 0 } else { metadata.len() };
        total = total
            .checked_add(
                entry_header_size(path)?
                    .checked_add(size)
                    .ok_or(StoreError::StorageFull)?,
            )
            .ok_or(StoreError::StorageFull)?;
        if total.checked_add(12).ok_or(StoreError::StorageFull)? > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
        let mut entry_header = Vec::with_capacity(12 + path.len());
        write_entry_header(&mut entry_header, path, directory, executable, size)?;
        writer.write_all(&entry_header).await?;
        if !directory {
            let mut file = open_nofollow(source, path)?;
            let mut remaining = size;
            while remaining > 0 {
                let wanted =
                    usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
                let count = file.read(&mut buffer[..wanted])?;
                if count == 0 {
                    return Err(StoreError::InvalidBuildCache);
                }
                writer.write_all(&buffer[..count]).await?;
                remaining -= count as u64;
            }
            let mut extra = [0; 1];
            if file.read(&mut extra)? != 0 {
                return Err(StoreError::InvalidBuildCache);
            }
        }
    }
    writer.write_all(&[0; 12]).await?;
    total = total.checked_add(12).ok_or(StoreError::StorageFull)?;
    if total != expected_size {
        return Err(StoreError::InvalidBuildCache);
    }
    Ok(total)
}

fn cache_paths(source: &Dir, exclusions: &[String]) -> Result<Vec<String>, StoreError> {
    let mut paths = Vec::new();
    let mut entries = 0usize;
    let mut path_bytes = 0usize;
    for root in CACHE_ROOTS {
        if is_excluded(root, exclusions) {
            continue;
        }
        match metadata_relative(source, root) {
            Ok(metadata) if metadata.is_dir() => collect_paths(
                source,
                root,
                exclusions,
                &mut paths,
                &mut entries,
                &mut path_bytes,
            )?,
            Ok(metadata) if metadata.is_file() => {
                entries = entries.checked_add(1).ok_or(StoreError::StorageFull)?;
                if entries > MAX_ARCHIVE_ENTRIES {
                    return Err(StoreError::StorageFull);
                }
                paths.push(root.to_string());
                path_bytes = path_bytes
                    .checked_add(root.len())
                    .ok_or(StoreError::StorageFull)?;
            }
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => continue,
            _ => return Err(StoreError::StateNotPrivate),
        }
    }
    if path_bytes > MAX_PATH_INDEX_BYTES {
        return Err(StoreError::StorageFull);
    }
    paths.sort_by(|left, right| compare_paths(left, right));
    paths.dedup();
    Ok(paths)
}

struct ArchiveReader<'a, R> {
    input: &'a mut R,
    last: Option<String>,
    total: u64,
    entries: usize,
    done: bool,
}

struct ArchiveEntry<'a, R> {
    input: &'a mut R,
    path: String,
    directory: bool,
    executable: bool,
    size: u64,
}

impl<'a, R: Read> ArchiveReader<'a, R> {
    fn new(input: &'a mut R, identity: &BuildCacheIdentity) -> Result<Self, StoreError> {
        let mut magic = [0; MAGIC.len()];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(StoreError::InvalidBuildCache);
        }
        let path_len = read_u16(input)? as usize;
        let device = read_u64(input)?;
        let inode = read_u64(input)?;
        if path_len == 0 || path_len > MAX_PATH_BYTES {
            return Err(StoreError::InvalidBuildCache);
        }
        let mut path = vec![0; path_len];
        input.read_exact(&mut path)?;
        if path != identity.path.as_bytes() || device != identity.device || inode != identity.inode
        {
            return Err(StoreError::InvalidBuildCache);
        }
        Ok(Self {
            input,
            last: None,
            total: header_size(identity)?,
            entries: 0,
            done: false,
        })
    }

    fn next(&mut self) -> Result<Option<ArchiveEntry<'_, R>>, StoreError> {
        if self.done {
            return Ok(None);
        }
        let path_len = read_u16(self.input)? as usize;
        let kind = read_u8(self.input)?;
        let flags = read_u8(self.input)?;
        let size = read_u64(self.input)?;
        if path_len == 0 {
            if kind != 0 || flags != 0 || size != 0 {
                return Err(StoreError::InvalidBuildCache);
            }
            self.done = true;
            self.total = self.total.checked_add(12).ok_or(StoreError::StorageFull)?;
            if self.total > MAX_ARCHIVE_BYTES {
                return Err(StoreError::StorageFull);
            }
            return Ok(None);
        }
        if path_len > MAX_PATH_BYTES {
            return Err(StoreError::InvalidBuildCache);
        }
        let mut path = vec![0; path_len];
        self.input.read_exact(&mut path)?;
        let path = String::from_utf8(path).map_err(|_| StoreError::InvalidBuildCache)?;
        validate_path(&path)?;
        let directory = match kind {
            0 if size == 0 => true,
            1 => false,
            _ => return Err(StoreError::InvalidBuildCache),
        };
        if flags & !1 != 0 || directory && flags != 0 || !allowed(&path) {
            return Err(StoreError::InvalidBuildCache);
        }
        if self
            .last
            .as_ref()
            .is_some_and(|last| compare_paths(last, &path) != std::cmp::Ordering::Less)
        {
            return Err(StoreError::InvalidBuildCache);
        }
        self.last = Some(path.clone());
        self.entries += 1;
        if self.entries > MAX_ARCHIVE_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let entry_size = entry_header_size(&path)?
            .checked_add(size)
            .ok_or(StoreError::StorageFull)?;
        self.total = self
            .total
            .checked_add(entry_size)
            .ok_or(StoreError::StorageFull)?;
        if self.total > MAX_ARCHIVE_BYTES {
            return Err(StoreError::StorageFull);
        }
        Ok(Some(ArchiveEntry {
            input: self.input,
            path,
            directory,
            executable: flags == 1,
            size,
        }))
    }

    fn validate_all(&mut self) -> Result<(), StoreError> {
        while let Some(mut entry) = self.next()? {
            entry.skip()?;
        }
        self.finish()
    }

    fn finish(&mut self) -> Result<(), StoreError> {
        if !self.done {
            return Err(StoreError::InvalidBuildCache);
        }
        let mut extra = [0; 1];
        if self.input.read(&mut extra)? != 0 {
            return Err(StoreError::InvalidBuildCache);
        }
        Ok(())
    }
}

impl<R: Read> ArchiveEntry<'_, R> {
    fn copy_to<W: Write>(&mut self, output: &mut W) -> Result<(), StoreError> {
        if io::copy(&mut self.input.take(self.size), output)? != self.size {
            return Err(StoreError::InvalidBuildCache);
        }
        Ok(())
    }

    fn skip(&mut self) -> Result<(), StoreError> {
        if io::copy(&mut self.input.take(self.size), &mut io::sink())? != self.size {
            return Err(StoreError::InvalidBuildCache);
        }
        Ok(())
    }
}

fn write_entry<R: Read, W: Write>(
    output: &mut W,
    entry: &mut ArchiveEntry<'_, R>,
    total: &mut u64,
    count: &mut usize,
) -> Result<(), StoreError> {
    *count += 1;
    if *count > MAX_ARCHIVE_ENTRIES {
        return Err(StoreError::StorageFull);
    }
    let entry_size = entry_header_size(&entry.path)?
        .checked_add(entry.size)
        .ok_or(StoreError::StorageFull)?;
    *total = total
        .checked_add(entry_size)
        .ok_or(StoreError::StorageFull)?;
    if *total > MAX_ARCHIVE_BYTES {
        return Err(StoreError::StorageFull);
    }
    write_entry_header(
        output,
        &entry.path,
        entry.directory,
        entry.executable,
        entry.size,
    )?;
    entry.copy_to(output)
}

fn collect_paths(
    source: &Dir,
    path: &str,
    exclusions: &[String],
    paths: &mut Vec<String>,
    entries: &mut usize,
    path_bytes: &mut usize,
) -> Result<(), StoreError> {
    if is_excluded(path, exclusions) {
        return Ok(());
    }
    validate_path(path)?;
    *entries = entries.checked_add(1).ok_or(StoreError::StorageFull)?;
    if *entries > MAX_ARCHIVE_ENTRIES {
        return Err(StoreError::StorageFull);
    }
    *path_bytes = path_bytes
        .checked_add(path.len())
        .ok_or(StoreError::StorageFull)?;
    if *path_bytes > MAX_PATH_INDEX_BYTES {
        return Err(StoreError::StorageFull);
    }
    paths.push(path.to_owned());
    let metadata = metadata_relative(source, path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let dir = open_relative_dir(source, &path.split('/').collect::<Vec<_>>())?;
    let mut names = Vec::new();
    for entry in dir.entries()? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        validate_component(&name)?;
        names.push(name);
        if names.len() > MAX_ARCHIVE_ENTRIES {
            return Err(StoreError::StorageFull);
        }
    }
    names.sort();
    for name in names {
        collect_paths(
            source,
            &format!("{path}/{name}"),
            exclusions,
            paths,
            entries,
            path_bytes,
        )?;
    }
    Ok(())
}

fn create_parent(root: &Dir, path: &str) -> Result<(Dir, String), StoreError> {
    validate_path(path)?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let leaf = parts.pop().ok_or(StoreError::InvalidBuildCache)?.to_owned();
    let mut dir = root.try_clone()?;
    let mut builder = DirBuilder::new();
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
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_ARCHIVE_BYTES {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(file)
}

fn cache_directory(state: &StateRoot, create: bool) -> Result<Dir, StoreError> {
    if create {
        let mut builder = DirBuilder::new();
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

fn lock_file(dir: &Dir, name: &str, try_only: bool) -> Result<File, StoreError> {
    let (file, _) = open_lock_file(dir, name)?;
    lock_exclusive(&file, try_only)?;
    Ok(file)
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
    check_private(&file, MAX_ARCHIVE_BYTES as usize)?;
    Ok(file)
}

fn open_regular(dir: &Dir, name: &str, max: u64) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let file = dir.open_with(name, &options)?;
    check_private(&file, max as usize)?;
    Ok(file)
}

fn check_private(file: &File, max: usize) -> Result<(), StoreError> {
    crate::private_fs::check_acl(file, true)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() > max as u64
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

fn inventory(dir: &Dir) -> Result<u64, StoreError> {
    let mut total = 0u64;
    let mut reservations = BTreeSet::new();
    let mut staging = BTreeSet::new();
    let mut candidates = BTreeSet::new();
    let mut previous = BTreeSet::new();
    let mut transactions = BTreeSet::new();
    let mut transaction_next = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut archives = Vec::new();
    let mut scanned = 0usize;
    for entry in dir.entries()? {
        scanned += 1;
        if scanned > MAX_DIRECTORY_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        if let Some(token) = suffix(&name, ARCHIVE_PREFIX, ".arc") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            archives.push(name);
        } else if let Some(token) = suffix(&name, PREVIOUS_PREFIX, ".arc") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            previous.insert(token.to_owned());
            archives.push(name);
        } else if let Some(token) = suffix(&name, RESERVATION_PREFIX, ".reserve") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            reservations.insert(token.to_owned());
        } else if let Some(token) = suffix(&name, STAGING_PREFIX, ".arc") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            staging.insert(token.to_owned());
        } else if let Some(token) = suffix(&name, CANDIDATE_PREFIX, ".arc") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            candidates.insert(token.to_owned());
        } else if let Some(token) = suffix(&name, IDENTITY_LOCK_PREFIX, ".lock") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            let lock = open_regular(dir, &name, 0)?;
            if lock.metadata()?.len() != 0 {
                return Err(StoreError::StateNotPrivate);
            }
        } else if let Some(token) = suffix(&name, TRANSACTION_NEXT_PREFIX, ".pending") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            transaction_next.insert(token.to_owned());
            let file = open_regular(dir, &name, 96)?;
            validate_transaction(&file, token)?;
        } else if let Some(token) = suffix(&name, TRANSACTION_PREFIX, ".pending") {
            validate_token(token)?;
            identities.insert(token.to_owned());
            transactions.insert(token.to_owned());
            let file = open_regular(dir, &name, 96)?;
            validate_transaction(&file, token)?;
        } else if name != NAMESPACE_LOCK && name != AGGREGATE_LOCK {
            return Err(StoreError::StateNotPrivate);
        }
    }
    if identities.len() > MAX_IDENTITIES {
        return Err(StoreError::StorageFull);
    }
    for name in archives {
        let file = open_regular(dir, &name, MAX_ARCHIVE_BYTES)?;
        total = total
            .checked_add(file.metadata()?.len())
            .ok_or(StoreError::StorageFull)?;
    }
    if staging.iter().any(|token| !reservations.contains(token))
        || candidates.iter().any(|token| !reservations.contains(token))
        || previous.iter().any(|token| !transactions.contains(token))
        || transactions
            .iter()
            .any(|token| !reservations.contains(token))
        || transaction_next
            .iter()
            .any(|token| !reservations.contains(token))
    {
        return Err(StoreError::StateNotPrivate);
    }
    for token in reservations {
        let reserve = open_regular(dir, &reservation_name(&token), 96)?;
        let reserved = validate_reservation(&reserve, &token)?;
        let staged_size = if staging.contains(&token) {
            open_regular(dir, &stage_name(&token), MAX_ARCHIVE_BYTES)?
                .metadata()?
                .len()
        } else {
            0
        };
        let candidate_size = if candidates.contains(&token) {
            open_regular(dir, &candidate_name(&token), MAX_ARCHIVE_BYTES)?
                .metadata()?
                .len()
        } else {
            0
        };
        if staged_size
            .checked_add(candidate_size)
            .ok_or(StoreError::StorageFull)?
            > reserved
        {
            return Err(StoreError::StorageFull);
        }
        total = total.checked_add(reserved).ok_or(StoreError::StorageFull)?;
    }
    total = total
        .checked_add((identities.len() as u64).saturating_mul(IDENTITY_METADATA_BYTES))
        .ok_or(StoreError::StorageFull)?;
    if total > AGGREGATE_BYTES {
        return Err(StoreError::StorageFull);
    }
    Ok(total)
}

fn remove_stale_candidates(dir: &Dir) -> Result<(), StoreError> {
    let mut stale = BTreeSet::new();
    let mut scanned = 0usize;
    for entry in dir.entries()? {
        scanned += 1;
        if scanned > MAX_DIRECTORY_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        let token = suffix(&name, RESERVATION_PREFIX, ".reserve")
            .or_else(|| suffix(&name, TRANSACTION_NEXT_PREFIX, ".pending"))
            .or_else(|| suffix(&name, TRANSACTION_PREFIX, ".pending"));
        let Some(token) = token else { continue };
        validate_token(token)?;
        stale.insert(token.to_owned());
    }
    for token in stale {
        let lock = open_regular(dir, &identity_lock_name(&token), 0)?;
        match lock_exclusive(&lock, true) {
            Ok(()) => recover_transaction(dir, &token)?,
            Err(StoreError::BuildCacheBusy) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn identity_count(dir: &Dir) -> Result<usize, StoreError> {
    let mut identities = BTreeSet::new();
    let mut scanned = 0usize;
    for entry in dir.entries()? {
        scanned += 1;
        if scanned > MAX_DIRECTORY_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| StoreError::StateNotPrivate)?;
        for (prefix, ending) in [
            (ARCHIVE_PREFIX, ".arc"),
            (PREVIOUS_PREFIX, ".arc"),
            (RESERVATION_PREFIX, ".reserve"),
            (STAGING_PREFIX, ".arc"),
            (CANDIDATE_PREFIX, ".arc"),
            (IDENTITY_LOCK_PREFIX, ".lock"),
            (TRANSACTION_NEXT_PREFIX, ".pending"),
            (TRANSACTION_PREFIX, ".pending"),
        ] {
            if let Some(token) = suffix(&name, prefix, ending) {
                validate_token(token)?;
                identities.insert(token.to_owned());
            }
        }
    }
    Ok(identities.len())
}

fn contains_cache_identity(dir: &Dir, token: &str) -> Result<bool, StoreError> {
    Ok(dir.symlink_metadata(archive_name(token)).is_ok()
        || dir.symlink_metadata(previous_name(token)).is_ok()
        || dir.symlink_metadata(reservation_name(token)).is_ok()
        || dir.symlink_metadata(stage_name(token)).is_ok()
        || dir.symlink_metadata(candidate_name(token)).is_ok()
        || dir.symlink_metadata(transaction_name(token)).is_ok()
        || dir.symlink_metadata(transaction_next_name(token)).is_ok()
        || dir.symlink_metadata(identity_lock_name(token)).is_ok())
}

fn remove_if_present(dir: &Dir, name: &str) -> Result<(), StoreError> {
    match dir.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheTransaction {
    RollbackPrevious,
    RollbackEmpty,
    CommittedPrevious,
    CommittedEmpty,
}

impl CacheTransaction {
    fn record(self) -> &'static str {
        match self {
            Self::RollbackPrevious => "rollback-previous",
            Self::RollbackEmpty => "rollback-empty",
            Self::CommittedPrevious => "committed-previous",
            Self::CommittedEmpty => "committed-empty",
        }
    }

    fn parse(record: &str) -> Option<Self> {
        match record {
            "rollback-previous" => Some(Self::RollbackPrevious),
            "rollback-empty" => Some(Self::RollbackEmpty),
            "committed-previous" => Some(Self::CommittedPrevious),
            "committed-empty" => Some(Self::CommittedEmpty),
            _ => None,
        }
    }
}

fn sync_directory(dir: &Dir) -> Result<(), StoreError> {
    dir.open_with(".", OpenOptions::new().read(true))?
        .sync_all()?;
    Ok(())
}

fn write_transaction(
    dir: &Dir,
    token: &str,
    transaction: CacheTransaction,
) -> Result<(), StoreError> {
    let next = transaction_next_name(token);
    let mut file = create_private_file(dir, &next)?;
    write!(file, "{token}\n{}\n", transaction.record())?;
    file.sync_all()?;
    drop(file);
    dir.rename(&next, dir, transaction_name(token))?;
    sync_directory(dir)
}

fn read_transaction(dir: &Dir, token: &str) -> Result<Option<CacheTransaction>, StoreError> {
    let file = match open_regular(dir, &transaction_name(token), 96) {
        Ok(file) => file,
        Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(96);
    file.take(97).read_to_end(&mut bytes)?;
    if bytes.len() > 96 {
        return Err(StoreError::StateNotPrivate);
    }
    let record = std::str::from_utf8(&bytes).map_err(|_| StoreError::StateNotPrivate)?;
    let mut lines = record.lines();
    if lines.next() != Some(token) {
        return Err(StoreError::StateNotPrivate);
    }
    let transaction = lines
        .next()
        .and_then(CacheTransaction::parse)
        .ok_or(StoreError::StateNotPrivate)?;
    if lines.next().is_some() || !record.ends_with('\n') {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(Some(transaction))
}

fn validate_transaction(file: &File, token: &str) -> Result<(), StoreError> {
    let copy = file.try_clone()?;
    let mut bytes = Vec::with_capacity(96);
    copy.take(97).read_to_end(&mut bytes)?;
    let record = std::str::from_utf8(&bytes).map_err(|_| StoreError::StateNotPrivate)?;
    let mut lines = record.lines();
    if lines.next() != Some(token)
        || lines.next().and_then(CacheTransaction::parse).is_none()
        || lines.next().is_some()
        || !record.ends_with('\n')
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

fn transaction_exists(dir: &Dir, name: &str) -> Result<bool, StoreError> {
    match dir.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn has_cache_files(dir: &Dir, token: &str) -> Result<bool, StoreError> {
    for name in [
        archive_name(token),
        previous_name(token),
        reservation_name(token),
        stage_name(token),
        candidate_name(token),
        transaction_name(token),
        transaction_next_name(token),
    ] {
        if transaction_exists(dir, &name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn set_reservation(dir: &Dir, token: &str, reserved: u64) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    nonblocking(&mut options);
    let mut file = dir.open_with(reservation_name(token), &options)?;
    check_private(&file, 96)?;
    write_reservation(&mut file, token, reserved)?;
    file.sync_all()?;
    Ok(())
}

fn validate_archive_for_token(file: &File, token: &str) -> Result<(), StoreError> {
    let mut reader = file.try_clone()?;
    reader.seek(std::io::SeekFrom::Start(0))?;
    let mut magic = [0; MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(StoreError::InvalidBuildCache);
    }
    let path_len = read_u16(&mut reader)? as usize;
    let device = read_u64(&mut reader)?;
    let inode = read_u64(&mut reader)?;
    if path_len == 0 || path_len > MAX_PATH_BYTES {
        return Err(StoreError::InvalidBuildCache);
    }
    let mut path = vec![0; path_len];
    reader.read_exact(&mut path)?;
    let path = String::from_utf8(path).map_err(|_| StoreError::InvalidBuildCache)?;
    let path_buf = Path::new(&path);
    if !valid_identity_path(path_buf) || inode == 0 {
        return Err(StoreError::InvalidBuildCache);
    }
    let identity = BuildCacheIdentity {
        path,
        device,
        inode,
    };
    if identity.token() != token {
        return Err(StoreError::InvalidBuildCache);
    }
    reader.seek(std::io::SeekFrom::Start(0))?;
    ArchiveReader::new(&mut reader, &identity)?.validate_all()
}

fn recover_transaction(dir: &Dir, token: &str) -> Result<(), StoreError> {
    recover_transaction_with_sync(dir, token, sync_directory)
}

fn recover_transaction_with_sync(
    dir: &Dir,
    token: &str,
    sync: impl Fn(&Dir) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let marker = transaction_name(token);
    let next = transaction_next_name(token);
    let Some(transaction) = read_transaction(dir, token)? else {
        remove_if_present(dir, &next)?;
        remove_if_present(dir, &stage_name(token))?;
        remove_if_present(dir, &candidate_name(token))?;
        remove_if_present(dir, &reservation_name(token))?;
        sync_directory(dir)?;
        return Ok(());
    };
    if !transaction_exists(dir, &reservation_name(token))? {
        return Err(StoreError::StateNotPrivate);
    }
    if matches!(
        transaction,
        CacheTransaction::CommittedPrevious | CacheTransaction::CommittedEmpty
    ) {
        sync(dir)?;
    }
    let archive = archive_name(token);
    let previous = previous_name(token);
    let has_previous = transaction_exists(dir, &previous)?;
    match transaction {
        CacheTransaction::RollbackPrevious if has_previous => {
            restore_previous(dir, token)?;
        }
        CacheTransaction::RollbackPrevious => {
            let current = open_regular(dir, &archive, MAX_ARCHIVE_BYTES)?;
            validate_archive_for_token(&current, token)?;
        }
        CacheTransaction::RollbackEmpty => {
            if has_previous {
                return Err(StoreError::StateNotPrivate);
            }
            remove_archive_file(dir, &archive)?;
        }
        CacheTransaction::CommittedPrevious if has_previous => {
            match open_regular(dir, &archive, MAX_ARCHIVE_BYTES) {
                Ok(current) if validate_archive_for_token(&current, token).is_ok() => {
                    let old = open_regular(dir, &previous, MAX_ARCHIVE_BYTES)?;
                    validate_archive_for_token(&old, token)?;
                    drop(old);
                    remove_if_present(dir, &previous)?;
                }
                Ok(_) => restore_previous(dir, token)?,
                Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => {
                    restore_previous(dir, token)?;
                }
                Err(error) => return Err(error),
            }
        }
        CacheTransaction::CommittedPrevious => {
            let current = open_regular(dir, &archive, MAX_ARCHIVE_BYTES)?;
            validate_archive_for_token(&current, token)?;
        }
        CacheTransaction::CommittedEmpty => {
            if has_previous {
                return Err(StoreError::StateNotPrivate);
            }
            match open_regular(dir, &archive, MAX_ARCHIVE_BYTES) {
                Ok(current) => validate_archive_for_token(&current, token)?,
                Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
    }
    sync(dir)?;
    remove_if_present(dir, &stage_name(token))?;
    remove_if_present(dir, &candidate_name(token))?;
    remove_if_present(dir, &marker)?;
    remove_if_present(dir, &next)?;
    sync(dir)?;
    remove_if_present(dir, &reservation_name(token))?;
    sync(dir)
}

fn restore_previous(dir: &Dir, token: &str) -> Result<(), StoreError> {
    let archive = archive_name(token);
    let previous = previous_name(token);
    let old = open_regular(dir, &previous, MAX_ARCHIVE_BYTES)?;
    validate_archive_for_token(&old, token)?;
    drop(old);
    remove_archive_file(dir, &archive)?;
    dir.rename(&previous, dir, &archive)?;
    Ok(())
}

fn remove_archive_file(dir: &Dir, name: &str) -> Result<(), StoreError> {
    match open_regular(dir, name, MAX_ARCHIVE_BYTES) {
        Ok(file) => {
            drop(file);
            dir.remove_file(name)?;
            Ok(())
        }
        Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

fn finish_committed(dir: &Dir, token: &str) -> Result<(), StoreError> {
    let transaction = read_transaction(dir, token)?.ok_or(StoreError::StateNotPrivate)?;
    if !matches!(
        transaction,
        CacheTransaction::CommittedPrevious | CacheTransaction::CommittedEmpty
    ) {
        return Err(StoreError::StateNotPrivate);
    }
    let archive = open_regular(dir, &archive_name(token), MAX_ARCHIVE_BYTES)?;
    validate_archive_for_token(&archive, token)?;
    let previous = previous_name(token);
    match (transaction, transaction_exists(dir, &previous)?) {
        (CacheTransaction::CommittedPrevious, true) => {
            let file = open_regular(dir, &previous, MAX_ARCHIVE_BYTES)?;
            validate_archive_for_token(&file, token)?;
        }
        (CacheTransaction::CommittedEmpty, true) => return Err(StoreError::StateNotPrivate),
        _ => {}
    }
    remove_if_present(dir, &previous)?;
    sync_directory(dir)?;
    remove_if_present(dir, &stage_name(token))?;
    remove_if_present(dir, &candidate_name(token))?;
    sync_directory(dir)?;
    remove_if_present(dir, &transaction_name(token))?;
    remove_if_present(dir, &transaction_next_name(token))?;
    sync_directory(dir)?;
    remove_if_present(dir, &reservation_name(token))?;
    sync_directory(dir)
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

fn validate_path(path: &str) -> Result<(), StoreError> {
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

fn write_header<W: Write>(writer: &mut W, identity: &BuildCacheIdentity) -> Result<(), StoreError> {
    writer.write_all(MAGIC)?;
    let path = identity.path.as_bytes();
    let length = u16::try_from(path.len()).map_err(|_| StoreError::StorageFull)?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&identity.device.to_le_bytes())?;
    writer.write_all(&identity.inode.to_le_bytes())?;
    writer.write_all(path)?;
    Ok(())
}

fn header_size(identity: &BuildCacheIdentity) -> Result<u64, StoreError> {
    u64::try_from(MAGIC.len() + 2 + 8 + 8 + identity.path.len())
        .map_err(|_| StoreError::StorageFull)
}

fn write_entry_header<W: Write>(
    writer: &mut W,
    path: &str,
    directory: bool,
    executable: bool,
    size: u64,
) -> Result<(), StoreError> {
    let length = u16::try_from(path.len()).map_err(|_| StoreError::StorageFull)?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&[if directory { 0 } else { 1 }, u8::from(executable)])?;
    writer.write_all(&size.to_le_bytes())?;
    writer.write_all(path.as_bytes())?;
    Ok(())
}

fn entry_header_size(path: &str) -> Result<u64, StoreError> {
    u64::try_from(12 + path.len()).map_err(|_| StoreError::StorageFull)
}

fn write_terminator<W: Write>(writer: &mut W) -> Result<(), StoreError> {
    writer.write_all(&[0; 12])?;
    Ok(())
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, StoreError> {
    let mut value = [0; 1];
    reader.read_exact(&mut value)?;
    Ok(value[0])
}
fn read_u16<R: Read>(reader: &mut R) -> Result<u16, StoreError> {
    let mut value = [0; 2];
    reader.read_exact(&mut value)?;
    Ok(u16::from_le_bytes(value))
}
fn read_u64<R: Read>(reader: &mut R) -> Result<u64, StoreError> {
    let mut value = [0; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_le_bytes(value))
}

async fn read_u8_async<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<u8, StoreError> {
    use tokio::io::AsyncReadExt;
    let mut value = [0; 1];
    reader.read_exact(&mut value).await?;
    Ok(value[0])
}

async fn read_u16_async<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<u16, StoreError> {
    use tokio::io::AsyncReadExt;
    let mut value = [0; 2];
    reader.read_exact(&mut value).await?;
    Ok(u16::from_le_bytes(value))
}

async fn read_u64_async<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<u64, StoreError> {
    use tokio::io::AsyncReadExt;
    let mut value = [0; 8];
    reader.read_exact(&mut value).await?;
    Ok(u64::from_le_bytes(value))
}

fn archive_name(token: &str) -> String {
    format!("{ARCHIVE_PREFIX}{token}.arc")
}
fn reservation_name(token: &str) -> String {
    format!("{RESERVATION_PREFIX}{token}.reserve")
}
fn previous_name(token: &str) -> String {
    format!("{PREVIOUS_PREFIX}{token}.arc")
}
fn stage_name(token: &str) -> String {
    format!("{STAGING_PREFIX}{token}.arc")
}
fn identity_lock_name(token: &str) -> String {
    format!("{IDENTITY_LOCK_PREFIX}{token}.lock")
}
fn candidate_name(token: &str) -> String {
    format!("{CANDIDATE_PREFIX}{token}.arc")
}
fn transaction_name(token: &str) -> String {
    format!("{TRANSACTION_PREFIX}{token}.pending")
}
fn transaction_next_name(token: &str) -> String {
    format!("{TRANSACTION_NEXT_PREFIX}{token}.pending")
}

fn write_reservation(file: &mut File, token: &str, reserved: u64) -> Result<(), StoreError> {
    if reserved != MAX_ARCHIVE_BYTES && reserved != RESERVED_TRANSIENT_BYTES {
        return Err(StoreError::StorageFull);
    }
    file.set_len(0)?;
    write!(file, "{token}\n{reserved}\n")?;
    Ok(())
}

fn validate_reservation(file: &File, token: &str) -> Result<u64, StoreError> {
    let mut bytes = Vec::with_capacity(96);
    file.try_clone()?.take(97).read_to_end(&mut bytes)?;
    let transient = format!("{token}\n{RESERVED_TRANSIENT_BYTES}\n");
    let archive = format!("{token}\n{MAX_ARCHIVE_BYTES}\n");
    if bytes == transient.as_bytes() {
        Ok(RESERVED_TRANSIENT_BYTES)
    } else if bytes == archive.as_bytes() {
        Ok(MAX_ARCHIVE_BYTES)
    } else {
        Err(StoreError::StateNotPrivate)
    }
}

fn compare_paths(left: &str, right: &str) -> std::cmp::Ordering {
    left.split('/').cmp(right.split('/'))
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

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;
    use std::os::unix::fs::DirBuilderExt;

    fn private_dir(path: &Path) {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .expect("private fixture directory");
    }

    fn identity(path: &Path) -> BuildCacheIdentity {
        let opened = crate::private_fs::open_directory(path, false).expect("Workspace admission");
        let metadata = opened.dir.dir_metadata().expect("Workspace metadata");
        BuildCacheIdentity::new(&opened.path, metadata.dev(), metadata.ino())
            .expect("Workspace cache identity")
    }

    fn archive(identity: &BuildCacheIdentity, source: &Path, exclusions: &[String]) -> Vec<u8> {
        let source = Dir::open_ambient_dir(source, ambient_authority()).expect("cache source");
        let mut bytes = Vec::new();
        capture_archive(&source, identity, exclusions, &mut bytes).expect("bounded archive");
        bytes
    }

    fn publish(
        state: &StateRoot,
        identity: BuildCacheIdentity,
        bytes: &[u8],
        exclusions: &[String],
    ) {
        let mut lease = state
            .begin_build_cache(identity)
            .expect("cache lease and quota reservation");
        let mut candidate_file = lease.candidate_writer().expect("candidate file");
        candidate_file.write_all(bytes).expect("candidate bytes");
        candidate_file.sync_all().expect("candidate sync");
        drop(candidate_file);
        let mut candidate = lease.open_candidate().expect("candidate reopen");
        lease
            .publish(&mut candidate, exclusions)
            .expect("atomic cache publish");
    }

    #[test]
    fn cache_stream_merge_preserves_only_selected_host_prefixes_and_clear_is_exact() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        private_dir(&state_path);
        private_dir(&workspace);
        private_dir(&other_workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let first = identity(&workspace);
        let second = identity(&other_workspace);
        let host_cache = workspace.join("initial/cargo/registry/cache/private.crate");
        let old_target = workspace.join("initial/target/debug/stale");
        std::fs::create_dir_all(host_cache.parent().expect("cache parent")).expect("cache dir");
        std::fs::create_dir_all(old_target.parent().expect("target parent")).expect("target dir");
        std::fs::write(&host_cache, b"private archive entry").expect("host path cache seed");
        std::fs::write(&old_target, b"stale output").expect("old target seed");
        let first_bytes = archive(&first, &workspace.join("initial"), &[]);
        publish(&state, first.clone(), &first_bytes, &[]);

        let selected = vec!["cargo/registry/cache".to_owned()];
        let updated = workspace.join("updated");
        let selected_host_path = updated.join("cargo/registry/cache/host.crate");
        let new_target = updated.join("target/debug/current");
        std::fs::create_dir_all(selected_host_path.parent().expect("selected parent"))
            .expect("selected cache dir");
        std::fs::create_dir_all(new_target.parent().expect("new target parent"))
            .expect("new target dir");
        std::fs::write(&selected_host_path, b"must not be imported").expect("host cache marker");
        std::fs::write(&new_target, b"current output").expect("current target");
        let updated_bytes = archive(&first, &updated, &selected);
        publish(&state, first.clone(), &updated_bytes, &selected);

        let lease = state
            .begin_build_cache(first.clone())
            .expect("reopen cache");
        let (mut stored, size) = lease.archive().expect("archive lookup").expect("archive");
        let mut stored_bytes = Vec::new();
        stored
            .read_to_end(&mut stored_bytes)
            .expect("stored archive");
        assert_eq!(stored_bytes.len() as u64, size);
        let restored = temp.path().join("restored");
        private_dir(&restored);
        let restored_dir = Dir::open_ambient_dir(&restored, ambient_authority()).expect("restore");
        restore_archive(&mut stored_bytes.as_slice(), &restored_dir, &first, &[])
            .expect("archive restore");
        assert_eq!(
            std::fs::read(restored.join("cargo/registry/cache/private.crate")).unwrap(),
            b"private archive entry"
        );
        assert_eq!(
            std::fs::read(restored.join("target/debug/current")).unwrap(),
            b"current output"
        );
        assert!(!restored.join("target/debug/stale").exists());
        assert!(!restored.join("cargo/registry/cache/host.crate").exists());
        drop(lease);

        publish(&state, second.clone(), &first_bytes, &[]);
        state
            .clear_workspace_build_cache(&workspace)
            .expect("clear current Workspace cache");
        assert!(
            state
                .begin_build_cache(first)
                .unwrap()
                .archive()
                .unwrap()
                .is_none()
        );
        state.clear_all_build_caches().expect("clear all caches");
        assert!(
            state
                .begin_build_cache(second)
                .unwrap()
                .archive()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interrupted_cache_swap_restores_the_previous_archive() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        private_dir(&state_path);
        private_dir(&workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let identity = identity(&workspace);
        let old_source = workspace.join("old-source");
        let new_source = workspace.join("new-source");
        std::fs::create_dir_all(old_source.join("target")).expect("old cache source");
        std::fs::create_dir_all(new_source.join("target")).expect("new cache source");
        std::fs::write(old_source.join("target/marker"), b"old committed archive")
            .expect("old cache data");
        std::fs::write(new_source.join("target/marker"), b"new candidate archive")
            .expect("new cache data");
        let old = archive(&identity, &old_source, &[]);
        let new = archive(&identity, &new_source, &[]);
        publish(&state, identity.clone(), &old, &[]);

        let lease = state
            .begin_build_cache(identity.clone())
            .expect("cache lease");
        let mut candidate = lease.candidate_writer().expect("candidate");
        candidate.write_all(&new).expect("candidate bytes");
        candidate.sync_all().expect("candidate sync");
        drop(candidate);
        let dir = cache_directory(&state, false).expect("cache directory");
        {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, false).expect("aggregate lock");
            let staged_name = stage_name(&identity.token());
            let mut staged = create_private_file(&dir, &staged_name).expect("staged archive");
            staged.write_all(&new).expect("staged bytes");
            staged.sync_all().expect("staged sync");
            write_transaction(&dir, &identity.token(), CacheTransaction::RollbackPrevious)
                .expect("rollback record");
            dir.rename(
                archive_name(&identity.token()),
                &dir,
                previous_name(&identity.token()),
            )
            .expect("move previous archive aside");
            sync_directory(&dir).expect("previous archive sync");
            dir.rename(&staged_name, &dir, archive_name(&identity.token()))
                .expect("install interrupted candidate");
            set_reservation(&dir, &identity.token(), MAX_ARCHIVE_BYTES)
                .expect("candidate reservation");
            sync_directory(&dir).expect("interrupted candidate sync");
        }
        drop(lease);

        let lease = state.begin_build_cache(identity).expect("recovered cache");
        let (mut recovered, size) = lease.archive().expect("archive lookup").expect("archive");
        let mut bytes = Vec::new();
        recovered
            .read_to_end(&mut bytes)
            .expect("recovered archive bytes");
        assert_eq!(bytes.len() as u64, size);
        assert_eq!(bytes, old);
    }

    #[test]
    fn committed_cache_swap_keeps_the_new_archive_during_recovery() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let old_workspace = temp.path().join("old-workspace");
        let first_workspace = temp.path().join("first-workspace");
        private_dir(&state_path);
        private_dir(&old_workspace);
        private_dir(&first_workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let old_identity = identity(&old_workspace);
        let first_identity = identity(&first_workspace);
        let old_source = temp.path().join("old-source");
        let new_source = temp.path().join("new-source");
        let first_source = temp.path().join("first-source");
        std::fs::create_dir_all(old_source.join("target")).expect("old cache source");
        std::fs::create_dir_all(new_source.join("target")).expect("new cache source");
        std::fs::create_dir_all(first_source.join("target")).expect("first cache source");
        std::fs::write(old_source.join("target/marker"), b"old archive").expect("old data");
        std::fs::write(new_source.join("target/marker"), b"new archive").expect("new data");
        std::fs::write(first_source.join("target/marker"), b"first archive").expect("first data");
        let old = archive(&old_identity, &old_source, &[]);
        let new = archive(&old_identity, &new_source, &[]);
        let first = archive(&first_identity, &first_source, &[]);
        publish(&state, old_identity.clone(), &old, &[]);

        let dir = cache_directory(&state, false).expect("cache directory");
        let lease = state
            .begin_build_cache(old_identity.clone())
            .expect("replacement lease");
        {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, false).expect("aggregate lock");
            let token = old_identity.token();
            let staged_name = stage_name(&token);
            let mut staged = create_private_file(&dir, &staged_name).expect("staged archive");
            staged.write_all(&new).expect("staged bytes");
            staged.sync_all().expect("staged sync");
            write_transaction(&dir, &token, CacheTransaction::RollbackPrevious)
                .expect("rollback record");
            dir.rename(archive_name(&token), &dir, previous_name(&token))
                .expect("move previous archive aside");
            dir.rename(&staged_name, &dir, archive_name(&token))
                .expect("install new archive");
            set_reservation(&dir, &token, MAX_ARCHIVE_BYTES).expect("candidate reservation");
            sync_directory(&dir).expect("new archive sync");
            write_transaction(&dir, &token, CacheTransaction::CommittedPrevious)
                .expect("commit record");
        }
        drop(lease);
        let lease = state
            .begin_build_cache(old_identity.clone())
            .expect("recovered replacement");
        let (mut stored, _) = lease.archive().expect("archive lookup").expect("archive");
        let mut recovered = Vec::new();
        stored
            .read_to_end(&mut recovered)
            .expect("recovered archive");
        assert_eq!(recovered, new);
        drop(lease);

        let lease = state
            .begin_build_cache(first_identity.clone())
            .expect("first cache lease");
        {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, false).expect("aggregate lock");
            let token = first_identity.token();
            let staged_name = stage_name(&token);
            let mut staged = create_private_file(&dir, &staged_name).expect("staged archive");
            staged.write_all(&first).expect("staged bytes");
            staged.sync_all().expect("staged sync");
            write_transaction(&dir, &token, CacheTransaction::RollbackEmpty)
                .expect("rollback record");
            dir.rename(&staged_name, &dir, archive_name(&token))
                .expect("install first archive");
            set_reservation(&dir, &token, MAX_ARCHIVE_BYTES).expect("candidate reservation");
            sync_directory(&dir).expect("first archive sync");
            write_transaction(&dir, &token, CacheTransaction::CommittedEmpty)
                .expect("commit record");
        }
        drop(lease);
        let lease = state
            .begin_build_cache(first_identity)
            .expect("recovered first archive");
        let (mut stored, _) = lease.archive().expect("archive lookup").expect("archive");
        let mut recovered = Vec::new();
        stored
            .read_to_end(&mut recovered)
            .expect("recovered archive");
        assert_eq!(recovered, first);
    }

    #[test]
    fn uncertain_commit_sync_preserves_both_archives_until_recovery() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        let old_source = temp.path().join("old-source");
        let new_source = temp.path().join("new-source");
        private_dir(&state_path);
        private_dir(&workspace);
        std::fs::create_dir_all(old_source.join("target")).expect("old cache source");
        std::fs::create_dir_all(new_source.join("target")).expect("new cache source");
        std::fs::write(old_source.join("target/marker"), b"old archive").expect("old data");
        std::fs::write(new_source.join("target/marker"), b"new archive").expect("new data");
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let identity = identity(&workspace);
        let old = archive(&identity, &old_source, &[]);
        let new = archive(&identity, &new_source, &[]);
        publish(&state, identity.clone(), &old, &[]);

        let dir = cache_directory(&state, false).expect("cache directory");
        let mut lease = state
            .begin_build_cache(identity.clone())
            .expect("replacement lease");
        let token = identity.token();
        {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, false).expect("aggregate lock");
            let staged_name = stage_name(&token);
            let mut staged = create_private_file(&dir, &staged_name).expect("staged archive");
            staged.write_all(&new).expect("staged bytes");
            staged.sync_all().expect("staged sync");
            write_transaction(&dir, &token, CacheTransaction::RollbackPrevious)
                .expect("rollback record");
            dir.rename(archive_name(&token), &dir, previous_name(&token))
                .expect("move previous archive aside");
            dir.rename(&staged_name, &dir, archive_name(&token))
                .expect("install new archive");
            set_reservation(&dir, &token, MAX_ARCHIVE_BYTES).expect("candidate reservation");
            sync_directory(&dir).expect("new archive sync");
            write_transaction(&dir, &token, CacheTransaction::CommittedPrevious)
                .expect("visible commit marker");
        }

        let result =
            recover_transaction_with_sync(&dir, &token, |_| Err(StoreError::StateNotPrivate));
        assert!(result.is_err());
        lease.defer_recovery = true;
        drop(lease);

        for (name, expected) in [
            (archive_name(&token), new.as_slice()),
            (previous_name(&token), old.as_slice()),
        ] {
            let mut stored = open_regular(&dir, &name, MAX_ARCHIVE_BYTES).expect("archive");
            validate_archive_for_token(&stored, &token).expect("valid retained archive");
            stored
                .seek(std::io::SeekFrom::Start(0))
                .expect("rewind archive");
            let mut bytes = Vec::new();
            stored.read_to_end(&mut bytes).expect("archive bytes");
            assert_eq!(bytes, expected);
        }
        assert_eq!(
            read_transaction(&dir, &token).expect("commit marker"),
            Some(CacheTransaction::CommittedPrevious)
        );

        {
            let _aggregate = lock_file(&dir, AGGREGATE_LOCK, false).expect("aggregate lock");
            recover_transaction(&dir, &token).expect("later durable recovery");
        }
        let mut stored = open_regular(&dir, &archive_name(&token), MAX_ARCHIVE_BYTES)
            .expect("committed archive");
        stored
            .seek(std::io::SeekFrom::Start(0))
            .expect("rewind archive");
        let mut bytes = Vec::new();
        stored.read_to_end(&mut bytes).expect("archive bytes");
        assert_eq!(bytes, new);
        assert!(!transaction_exists(&dir, &previous_name(&token)).expect("backup check"));
    }

    #[test]
    fn one_workspace_cache_lease_excludes_a_second_command() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        private_dir(&state_path);
        private_dir(&workspace);
        private_dir(&other_workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let workspace_identity = identity(&workspace);
        let lease = state
            .begin_build_cache(workspace_identity.clone())
            .expect("first cache lease");
        assert!(matches!(
            state.begin_build_cache(workspace_identity.clone()),
            Err(StoreError::BuildCacheBusy)
        ));
        let dir = cache_directory(&state, false).expect("cache directory");
        let _aggregate = lock_file(&dir, AGGREGATE_LOCK, true).expect("aggregate lock");
        assert!(matches!(
            state.begin_build_cache(identity(&other_workspace)),
            Err(StoreError::BuildCacheBusy)
        ));
        drop(_aggregate);
        drop(lease);
        assert!(state.begin_build_cache(workspace_identity).is_ok());
    }

    #[test]
    fn cache_reservation_refuses_whole_state_root_overflow() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        private_dir(&state_path);
        private_dir(&workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let dir = cache_directory(&state, true).expect("cache directory");
        let mut held = Vec::new();
        for index in 0..65 {
            let token = format!("{index:064x}");
            let (lock, _) = open_lock_file(&dir, &identity_lock_name(&token)).expect("lock file");
            lock_exclusive(&lock, false).expect("active cache owner");
            let mut reservation =
                create_private_file(&dir, &reservation_name(&token)).expect("reservation");
            write_reservation(&mut reservation, &token, RESERVED_TRANSIENT_BYTES)
                .expect("reservation record");
            reservation.sync_all().expect("reservation sync");
            held.push((lock, reservation));
        }
        assert!(matches!(
            state.begin_build_cache(identity(&workspace)),
            Err(StoreError::StorageFull)
        ));
        drop(held);
    }

    #[test]
    fn cache_identity_cap_is_independent_of_workspace_trust() {
        let temp = tempfile::tempdir().expect("fixture");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        private_dir(&state_path);
        private_dir(&workspace);
        let state = StateRoot::admit(&state_path).expect("StateRoot");
        let dir = cache_directory(&state, true).expect("cache directory");
        for index in 0..MAX_IDENTITIES {
            let token = format!("{index:064x}");
            let (lock, _) = open_lock_file(&dir, &identity_lock_name(&token)).expect("lock file");
            drop(lock);
        }
        assert!(matches!(
            state.begin_build_cache(identity(&workspace)),
            Err(StoreError::StorageFull)
        ));
    }
}

use super::*;
use std::collections::BTreeSet;
pub(super) mod guard;
pub(crate) use guard::CacheGuardRecovery;
#[cfg(test)]
mod tests;

pub(crate) enum CacheChange {
    Remove(String),
    Directory(String),
    File(String),
}

impl BuildCacheLease {
    pub(crate) fn prepare_directory(&mut self) -> Result<Dir, StoreError> {
        directory_available(&self.dir, &self.token)?;
        let name = tree_name(&self.token);
        if !file_exists(&self.dir, &name)? {
            let mut builder = cap_std::fs::DirBuilder::new();
            cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
            self.dir.create_dir_with(&name, &builder)?;
            sync_directory(&self.dir)?;
        }
        Ok(self.dir.open_dir_nofollow(&name)?)
    }

    // Applies in place only while the result fits the candidate limit and the StateRoot volume keeps
    // `reserve` free, after evicting least-recently-used unleased caches.
    pub(crate) fn publish_directory(
        &mut self,
        lower: &Dir,
        upper: &Dir,
        changes: &[CacheChange],
        exclusions: &[String],
        reserve: u64,
    ) -> Result<(), StoreError> {
        let current = self.dir.open_dir_nofollow(tree_name(&self.token))?;
        let pinned = lower.dir_metadata()?;
        let now = current.dir_metadata()?;
        if (pinned.dev(), pinned.ino()) != (now.dev(), now.ino()) {
            return Err(StoreError::StateNotPrivate);
        }
        directory_available(&self.dir, &self.token)?;
        for change in changes {
            let path = match change {
                CacheChange::Remove(path)
                | CacheChange::Directory(path)
                | CacheChange::File(path) => path,
            };
            validate_path(path)?;
            if is_excluded(path, exclusions) {
                return Err(StoreError::InvalidBuildCache);
            }
        }
        let growth = self.preflight_directory(lower, upper, changes)?;
        let floor = reserve.saturating_add(growth);
        if evict_until_free(&self.dir, floor)? < floor {
            return Err(StoreError::StorageFull);
        }
        mark_unavailable(&self.dir, &self.token)?;
        self.uncertain = true;
        apply(lower, upper, changes)?;
        sync_changes(lower, changes, &self.token)?;
        self.finish_directory()?;
        self.uncertain = false;
        Ok(())
    }

    fn finish_directory(&self) -> Result<(), StoreError> {
        self.dir.remove_file(dirty_name(&self.token))?;
        if let Err(error) = sync_directory(&self.dir) {
            let _ = mark_unavailable(&self.dir, &self.token);
            return Err(error);
        }
        Ok(())
    }

    // Returns how many bytes the applied changes add to the committed cache.
    fn preflight_directory(
        &self,
        lower: &Dir,
        upper: &Dir,
        changes: &[CacheChange],
    ) -> Result<u64, StoreError> {
        let before = measure(lower)?;
        let mut after = before;
        for change in changes {
            match change {
                CacheChange::Remove(path) => after = after.saturating_sub(file_bytes(lower, path)?),
                CacheChange::Directory(_) => {}
                CacheChange::File(path) => {
                    after = after
                        .saturating_sub(file_bytes(lower, path)?)
                        .checked_add(metadata_relative(upper, path)?.len())
                        .ok_or(StoreError::StorageFull)?;
                }
            }
        }
        if after > self.candidate_limit {
            return Err(StoreError::StorageFull);
        }
        Ok(after.saturating_sub(before))
    }
}

fn file_bytes(tree: &Dir, path: &str) -> Result<u64, StoreError> {
    match metadata_relative(tree, path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => Ok(0),
        Err(error) => Err(error),
    }
}

fn apply(lower: &Dir, upper: &Dir, changes: &[CacheChange]) -> Result<(), StoreError> {
    for change in changes {
        match change {
            CacheChange::Remove(path) => {
                let (parent, leaf) = create_parent(lower, path)?;
                match parent.symlink_metadata(&leaf) {
                    Ok(metadata) if metadata.is_dir() => parent.remove_dir(&leaf)?,
                    Ok(metadata) if metadata.is_file() => parent.remove_file(&leaf)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => return Err(StoreError::StateNotPrivate),
                }
                sync_directory(&parent)?;
            }
            CacheChange::Directory(path) => {
                let (parent, leaf) = create_parent(lower, path)?;
                if !file_exists(&parent, &leaf)? {
                    let mut builder = cap_std::fs::DirBuilder::new();
                    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
                    parent.create_dir_with(&leaf, &builder)?;
                }
            }
            CacheChange::File(path) => {
                let mut source = open_nofollow(upper, path)?;
                let metadata = source.metadata()?;
                check_source_file(&metadata)?;
                let (parent, leaf) = create_parent(lower, path)?;
                remove_if_present(&parent, &leaf)?;
                let target = publish_file(&mut source, metadata.len(), &parent, &leaf)?;
                use std::os::unix::fs::PermissionsExt;
                target.set_permissions(cap_std::fs::Permissions::from_std(
                    std::fs::Permissions::from_mode(if metadata.mode() & 0o111 == 0 {
                        0o600
                    } else {
                        0o700
                    }),
                ))?;
                // Cargo compares fingerprint mtimes, so a copy must keep the payload's times.
                rustix::fs::futimens(
                    &target,
                    &rustix::fs::Timestamps {
                        last_access: timespec(metadata.atime(), metadata.atime_nsec()),
                        last_modification: timespec(metadata.mtime(), metadata.mtime_nsec()),
                    },
                )
                .map_err(|error| StoreError::Io(error.into()))?;
                rustix::fs::fsync(&target).map_err(|error| StoreError::Io(error.into()))?;
            }
        }
    }
    Ok(())
}

fn timespec(seconds: i64, nanoseconds: i64) -> rustix::fs::Timespec {
    rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds,
    }
}

// A macOS view shares the Store's volume, so a clone publishes without a second copy on disk.
fn publish_file(source: &mut File, len: u64, parent: &Dir, leaf: &str) -> Result<File, StoreError> {
    #[cfg(target_os = "macos")]
    if crate::private_fs::clone_at(source, parent, leaf).is_ok() {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        nonblocking(&mut options);
        let target = parent.open_with(leaf, &options)?;
        // A clone keeps the payload's mode; restrict it before the private-file check.
        use std::os::unix::fs::PermissionsExt;
        target.set_permissions(cap_std::fs::Permissions::from_std(
            std::fs::Permissions::from_mode(0o600),
        ))?;
        check_private(&target, len)?;
        if target.metadata()?.len() != len {
            return Err(StoreError::InvalidBuildCache);
        }
        return Ok(target);
    }
    let mut target = create_private_file(parent, leaf)?;
    if io::copy(&mut io::Read::take(source, len + 1), &mut target)? != len {
        return Err(StoreError::InvalidBuildCache);
    }
    Ok(target)
}

pub(super) fn directory_available(dir: &Dir, token: &str) -> Result<(), StoreError> {
    if file_exists(dir, &dirty_name(token))? {
        return Err(StoreError::InvalidBuildCache);
    }
    Ok(())
}

fn mark_unavailable(dir: &Dir, token: &str) -> Result<(), StoreError> {
    create_private_file(dir, &dirty_name(token))?.sync_all()?;
    sync_directory(dir)
}

// Logical bytes of the regular files under the cache roots.
pub(super) fn measure(tree: &Dir) -> Result<u64, StoreError> {
    let mut bytes = 0u64;
    walk(tree, &mut |metadata| {
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
        Ok(())
    })?;
    Ok(bytes)
}

// `apply` fsyncs each file and this each changed directory, deepest first; the root's full sync then
// flushes the device cache once for all of them (per-entry F_FULLFSYNC costs milliseconds on macOS).
fn sync_changes(tree: &Dir, changes: &[CacheChange], token: &str) -> Result<(), StoreError> {
    let mut directories = BTreeSet::new();
    for change in changes {
        let (path, own) = match change {
            CacheChange::Directory(path) => (path, true),
            CacheChange::Remove(path) | CacheChange::File(path) => (path, false),
        };
        let path = Path::new(path);
        directories.extend(
            path.ancestors()
                .skip(usize::from(!own))
                .filter(|path| !path.as_os_str().is_empty()),
        );
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in directories {
        let path = path.to_str().ok_or(StoreError::InvalidBuildCache)?;
        let parts = path.split('/').collect::<Vec<_>>();
        let directory = match open_relative_dir(tree, &parts) {
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => continue,
            directory => directory?,
        };
        crate::fault::record_sync(Path::new(token), path)?;
        // A Linux `Dir` handle is `O_PATH`, which cannot be synced.
        let readable = directory.open_with(".", cap_std::fs::OpenOptions::new().read(true))?;
        rustix::fs::fsync(&readable).map_err(|error| StoreError::Io(error.into()))?;
    }
    crate::fault::record_sync(Path::new(token), "")?;
    sync_directory(tree)
}

pub(super) fn tree_name(token: &str) -> String {
    format!("{TREE_PREFIX}{token}.tree")
}
fn dirty_name(token: &str) -> String {
    format!("{DIRTY_PREFIX}{token}.pending")
}

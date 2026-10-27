use super::*;
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
        let _aggregate = lock_file(&self.dir, AGGREGATE_LOCK, true)?;
        directory_available(&self.dir, &self.token)?;
        if self.candidate_limit == 0 {
            return Err(StoreError::StorageFull);
        }
        let lower_bytes = own_tree_bytes(&self.dir, &self.token)?.max(
            self.archive
                .as_ref()
                .map(|archive| archive.metadata().map(|metadata| metadata.len()))
                .transpose()?
                .unwrap_or_default(),
        );
        let reserved = self
            .archive_limit
            .checked_add(self.candidate_limit)
            .and_then(|headroom| headroom.checked_add(lower_bytes))
            .ok_or(StoreError::StorageFull)?;
        let reservation = open_regular(&self.dir, &reservation_name(&self.token), 96)?;
        let previous = validate_reservation(&reservation, &self.token)?;
        if inventory(&self.dir)?
            .saturating_sub(previous)
            .checked_add(reserved)
            .ok_or(StoreError::StorageFull)?
            > AGGREGATE_BYTES
        {
            return Err(StoreError::StorageFull);
        }
        set_reservation(&self.dir, &self.token, reserved)?;
        sync_directory(&self.dir)?;
        let name = tree_name(&self.token);
        let stamp = stamp_name(&self.token);
        let migrate = file_exists(&self.dir, &stamp)?;
        if migrate
            && open_regular(&self.dir, &stamp, GENERATION_BYTES as u64)?
                .metadata()?
                .len()
                != GENERATION_BYTES as u64
        {
            return Err(StoreError::InvalidBuildCache);
        }
        if file_exists(&self.dir, &name)? && !migrate {
            let tree = self.dir.open_dir_nofollow(&name)?;
            measure(&tree, self.archive_limit)?;
            return Ok(tree);
        }
        mark_unavailable(&self.dir, &self.token)?;
        if file_exists(&self.dir, &name)? {
            self.dir.remove_dir_all(&name)?;
        }
        let mut builder = DirBuilder::new();
        cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
        self.dir.create_dir_with(&name, &builder)?;
        let tree = self.dir.open_dir_nofollow(&name)?;
        if let Some((mut archive, _)) = self.archive()? {
            restore_archive_with_limit(
                &mut archive,
                &tree,
                &self.identity,
                &[],
                self.archive_limit,
            )?;
        }
        measure(&tree, self.archive_limit)?;
        sync_tree(&tree, &self.token)?;
        self.archive = None;
        self.finish_directory()?;
        Ok(tree)
    }

    pub(crate) fn publish_directory(
        &mut self,
        lower: &Dir,
        upper: &Dir,
        changes: &[CacheChange],
        exclusions: &[String],
    ) -> Result<(), StoreError> {
        if changes.len() > MAX_ARCHIVE_ENTRIES * 2 {
            return Err(StoreError::StorageFull);
        }
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
        self.preflight_directory(lower, upper, changes)?;
        mark_unavailable(&self.dir, &self.token)?;
        self.defer_recovery = true;
        apply(lower, upper, changes)?;
        measure(lower, self.candidate_limit)?;
        sync_tree(lower, &self.token)?;
        self.archive = None;
        self.finish_directory()?;
        self.defer_recovery = false;
        Ok(())
    }

    fn finish_directory(&self) -> Result<(), StoreError> {
        remove_if_present(&self.dir, &archive_name(&self.token))?;
        remove_if_present(&self.dir, &stamp_name(&self.token))?;
        sync_directory(&self.dir)?;
        self.dir.remove_file(dirty_name(&self.token))?;
        if let Err(error) = sync_directory(&self.dir) {
            let _ = mark_unavailable(&self.dir, &self.token);
            return Err(error);
        }
        Ok(())
    }

    fn preflight_directory(
        &self,
        lower: &Dir,
        upper: &Dir,
        changes: &[CacheChange],
    ) -> Result<(), StoreError> {
        let mut paths = cache_paths(lower, &[])?
            .into_iter()
            .map(|path| {
                let metadata = metadata_relative(lower, &path)?;
                Ok((
                    path,
                    if metadata.is_file() {
                        metadata.len()
                    } else {
                        0
                    },
                ))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, StoreError>>()?;
        for change in changes {
            match change {
                CacheChange::Remove(path) => {
                    paths.remove(path);
                }
                CacheChange::Directory(path) => {
                    paths.insert(path.clone(), 0);
                }
                CacheChange::File(path) => {
                    paths.insert(path.clone(), metadata_relative(upper, path)?.len());
                }
            }
        }
        if paths.len() > MAX_ARCHIVE_ENTRIES {
            return Err(StoreError::StorageFull);
        }
        let mut bytes = header_size(&self.identity)?;
        for (path, size) in paths {
            bytes = account_entry(bytes, &path, size, self.candidate_limit)?;
        }
        Ok(())
    }
}

pub(super) fn committed_bytes(
    dir: &Dir,
    token: &str,
    archive_bytes: u64,
) -> Result<u64, StoreError> {
    let name = tree_name(token);
    if file_exists(dir, &name)? {
        Ok(archive_bytes.max(measure(&dir.open_dir_nofollow(name)?, MAX_ARCHIVE_BYTES)?))
    } else {
        Ok(archive_bytes)
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
                    let mut builder = DirBuilder::new();
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
                let mut target = create_private_file(&parent, &leaf)?;
                if io::copy(&mut (&mut source).take(metadata.len() + 1), &mut target)?
                    != metadata.len()
                {
                    return Err(StoreError::InvalidBuildCache);
                }
                use std::os::unix::fs::PermissionsExt;
                target.set_permissions(cap_std::fs::Permissions::from_std(
                    std::fs::Permissions::from_mode(if metadata.mode() & 0o111 == 0 {
                        0o600
                    } else {
                        0o700
                    }),
                ))?;
                target.sync_all()?;
            }
        }
    }
    Ok(())
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

pub(super) fn measure(tree: &Dir, limit: u64) -> Result<u64, StoreError> {
    let mut bytes = 0u64;
    for path in cache_paths(tree, &[])? {
        bytes = bytes
            .checked_add(entry_header_size(&path)?)
            .ok_or(StoreError::StorageFull)?;
        let metadata = metadata_relative(tree, &path)?;
        if !metadata.is_dir() {
            check_source_file(&metadata)?;
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or(StoreError::StorageFull)?;
        }
        if bytes > limit {
            return Err(StoreError::StorageFull);
        }
    }
    Ok(bytes)
}

fn sync_tree(tree: &Dir, token: &str) -> Result<(), StoreError> {
    let paths = cache_paths(tree, &[])?;
    for path in paths.iter().rev() {
        if metadata_relative(tree, path)?.is_dir() {
            sync_cache_directory(tree, path, token)?;
        } else {
            open_nofollow(tree, path)?.sync_all()?;
        }
    }
    let ancestors = CACHE_ROOTS
        .iter()
        .flat_map(|root| Path::new(root).ancestors().skip(1))
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    let mut ancestors = ancestors.into_iter().collect::<Vec<_>>();
    ancestors.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in ancestors {
        match sync_cache_directory(
            tree,
            path.to_str().ok_or(StoreError::InvalidBuildCache)?,
            token,
        ) {
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => {}
            result => result?,
        }
    }
    sync_cache_directory(tree, "", token)
}

fn sync_cache_directory(tree: &Dir, path: &str, token: &str) -> Result<(), StoreError> {
    let parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let directory = open_relative_dir(tree, &parts)?;
    crate::fault::record_sync(Path::new(token), path)?;
    sync_directory(&directory)
}

pub(super) fn tree_name(token: &str) -> String {
    format!("{TREE_PREFIX}{token}.tree")
}
fn dirty_name(token: &str) -> String {
    format!("{DIRTY_PREFIX}{token}.pending")
}
fn stamp_name(token: &str) -> String {
    format!("{STAMP_PREFIX}{token}.generation")
}

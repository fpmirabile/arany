use super::*;
pub(super) mod guard;
pub(crate) use guard::CacheGuardRecovery;
#[cfg(test)]
mod tests;

impl BuildCacheLease {
    pub(crate) fn prepare_directory(&self) -> Result<Dir, StoreError> {
        let _aggregate = lock_file(&self.dir, AGGREGATE_LOCK, true)?;
        directory_available(&self.dir, &self.token)?;
        if self.shared || self.candidate_limit == 0 {
            return Err(StoreError::StorageFull);
        }
        let extra = self
            .archive_limit
            .checked_mul(2)
            .ok_or(StoreError::StorageFull)?;
        if inventory(&self.dir)?
            .checked_add(extra)
            .ok_or(StoreError::StorageFull)?
            > AGGREGATE_BYTES
        {
            return Err(StoreError::StorageFull);
        }
        set_reservation(&self.dir, &self.token, self.candidate_limit * 2 + extra)?;
        sync_directory(&self.dir)?;
        let name = tree_name(&self.token);
        let generation = self.generation()?.unwrap_or_default();
        let stamp = stamp_name(&self.token);
        if file_exists(&self.dir, &stamp)? {
            let mut bytes = Vec::new();
            open_regular(&self.dir, &stamp, GENERATION_BYTES as u64)?
                .take(GENERATION_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() != GENERATION_BYTES {
                return Err(StoreError::InvalidBuildCache);
            }
            if bytes == generation {
                let tree = self.dir.open_dir_nofollow(&name)?;
                measure(&tree, self.archive_limit)?;
                return Ok(tree);
            }
        } else if file_exists(&self.dir, &name)? {
            return Err(StoreError::InvalidBuildCache);
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
        self.finish_directory()?;
        Ok(tree)
    }

    fn finish_directory(&self) -> Result<(), StoreError> {
        let stamp = stamp_name(&self.token);
        remove_if_present(&self.dir, &stamp)?;
        let mut file = create_private_file(&self.dir, &stamp)?;
        file.write_all(&self.generation()?.unwrap_or_default())?;
        file.sync_all()?;
        sync_directory(&self.dir)?;
        self.dir.remove_file(dirty_name(&self.token))?;
        if let Err(error) = sync_directory(&self.dir) {
            let _ = mark_unavailable(&self.dir, &self.token);
            return Err(error);
        }
        Ok(())
    }
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

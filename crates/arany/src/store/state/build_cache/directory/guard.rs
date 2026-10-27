use super::*;

pub(in crate::store::state::build_cache) const GUARD_BYTES: u64 =
    uuid::fmt::Hyphenated::LENGTH as u64;

pub(crate) struct CacheGuardRecovery {
    dir: Dir,
    token: String,
    _lease: FlockGuard,
    pub(crate) id: uuid::Uuid,
}

fn name(token: &str) -> String {
    format!("{GUARD_PREFIX}{token}.unit")
}

pub(in crate::store::state::build_cache) fn active(
    dir: &Dir,
    token: &str,
) -> Result<bool, StoreError> {
    file_exists(dir, &name(token))
}

pub(in crate::store::state::build_cache) fn admit(
    dir: &Dir,
    token: &str,
) -> Result<(), StoreError> {
    if active(dir, token)? {
        return Err(StoreError::BuildCacheBusy);
    }
    Ok(())
}

impl CacheGuardRecovery {
    pub(crate) fn complete(self) -> Result<(), StoreError> {
        self.dir.remove_file(name(&self.token))?;
        sync_directory(&self.dir)
    }
}

impl BuildCacheLease {
    pub(crate) fn guard_started(&self, id: uuid::Uuid) -> Result<(), StoreError> {
        let mut file = create_private_file(&self.dir, &name(&self.token))?;
        write!(file, "{id}")?;
        file.sync_all()?;
        sync_directory(&self.dir)
    }

    pub(crate) fn guard_finished(&self) -> Result<(), StoreError> {
        remove_if_present(&self.dir, &name(&self.token))?;
        sync_directory(&self.dir)
    }
}

impl StateRoot {
    pub(crate) fn cache_guard_recoveries(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<CacheGuardRecovery>, StoreError> {
        let dir = match cache_directory(self, false) {
            Ok(dir) => dir,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let selected = workspace
            .map(|path| {
                let root = crate::private_fs::open_workspace_directory(path)?;
                self.ensure_outside_workspace(&root)?;
                let metadata = root.dir_metadata()?;
                BuildCacheIdentity::new(path, metadata.dev(), metadata.ino())
                    .map(|identity| identity.token())
            })
            .transpose()?;
        let _namespace = lock_file(&dir, NAMESPACE_LOCK, true)?;
        let mut recoveries = Vec::new();
        for (count, entry) in dir.entries()?.enumerate() {
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::StorageFull);
            }
            let entry = entry?
                .file_name()
                .into_string()
                .map_err(|_| StoreError::StateNotPrivate)?;
            let Some(token) = suffix(&entry, GUARD_PREFIX, ".unit") else {
                continue;
            };
            validate_token(token)?;
            if selected.as_ref().is_some_and(|selected| selected != token) {
                continue;
            }
            let lease = lock_file(&dir, &identity_lock_name(token), true)?;
            let mut bytes = String::new();
            open_regular(&dir, &entry, GUARD_BYTES)?
                .take(GUARD_BYTES + 1)
                .read_to_string(&mut bytes)?;
            let id = uuid::Uuid::parse_str(&bytes).map_err(|_| StoreError::InvalidBuildCache)?;
            if id.is_nil() || id.to_string() != bytes {
                return Err(StoreError::InvalidBuildCache);
            }
            recoveries.push(CacheGuardRecovery {
                dir: dir.try_clone()?,
                token: token.into(),
                _lease: lease,
                id,
            });
            if recoveries.len() > MAX_IDENTITIES {
                return Err(StoreError::StorageFull);
            }
        }
        Ok(recoveries)
    }
}

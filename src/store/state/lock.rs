#[cfg(unix)]
use super::nonblocking;
use super::{StateRoot, StoreError};
use crate::session::SessionId;
#[cfg(unix)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(unix)]
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptions;
use cap_std::fs::{Dir, File};
#[cfg(unix)]
use std::io;
use std::path::Path;

#[cfg(unix)]
const LOCK_NAMESPACE_FILE: &str = "session-locks.guard";
#[cfg(unix)]
const ACCOUNT_LOCK_FILE: &str = "account-credentials.lock";
#[cfg(unix)]
const MAX_SESSION_LOCK_FILES: usize = 65_536;
#[cfg(unix)]
const MAX_STATE_OVERHEAD_FILES: usize = 32;

#[cfg(unix)]
struct NamespaceLock(File);

#[cfg(unix)]
impl Drop for NamespaceLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

#[cfg(unix)]
struct AccountLock(File);

#[cfg(unix)]
impl Drop for AccountLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

pub(crate) struct SessionRunLock {
    file: File,
    dir: Dir,
    name: String,
    remove_on_drop: bool,
}

impl SessionRunLock {
    pub(crate) fn keep(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for SessionRunLock {
    fn drop(&mut self) {
        if self.remove_on_drop {
            #[cfg(unix)]
            if let Ok(_namespace) = lock_namespace(&self.dir) {
                remove_matching_file(&self.dir, &self.name, &self.file);
            }
            #[cfg(not(unix))]
            let _ = self.dir.remove_file(&self.name);
        }
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

impl StateRoot {
    pub fn with_account_replacement_lock<T>(
        &self,
        workspace: &Path,
        operation: impl FnOnce() -> T,
    ) -> Result<T, StoreError> {
        #[cfg(unix)]
        {
            let workspace = Dir::open_ambient_dir(workspace, ambient_authority())?;
            self.ensure_outside_workspace(&workspace)?;
            let _lock = lock_account(self)?;
            Ok(operation())
        }
        #[cfg(not(unix))]
        {
            let _ = (workspace, operation);
            Err(StoreError::InvalidStateDirectory)
        }
    }

    pub(crate) fn lock_session(&self, session_id: SessionId) -> Result<SessionRunLock, StoreError> {
        #[cfg(unix)]
        {
            lock_session(self, session_id)
        }
        #[cfg(not(unix))]
        {
            let _ = session_id;
            Err(StoreError::InvalidStateDirectory)
        }
    }
}

#[cfg(unix)]
fn lock_account(state: &StateRoot) -> Result<AccountLock, StoreError> {
    use rustix::fs::{FlockOperation, flock};

    let (file, _) = open_private_lock_file(&state._dir, ACCOUNT_LOCK_FILE, || Ok(()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(AccountLock(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(StoreError::AccountBusy),
        Err(error) => Err(StoreError::Io(io::Error::from_raw_os_error(
            error.raw_os_error(),
        ))),
    }
}

#[cfg(unix)]
fn lock_session(state: &StateRoot, session_id: SessionId) -> Result<SessionRunLock, StoreError> {
    lock_session_with_limit(state, session_id, MAX_SESSION_LOCK_FILES)
}

#[cfg(unix)]
fn lock_session_with_limit(
    state: &StateRoot,
    session_id: SessionId,
    limit: usize,
) -> Result<SessionRunLock, StoreError> {
    use rustix::fs::{FlockOperation, flock};

    let name = format!("session-{session_id}.lock");
    let dir = state._dir.try_clone()?;
    let _namespace = lock_namespace(&dir)?;
    let (file, created) =
        open_private_lock_file(&dir, &name, || check_lock_file_quota(&dir, limit))?;
    let result = match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(StoreError::SessionBusy),
        Err(error) => Err(StoreError::Io(io::Error::from_raw_os_error(
            error.raw_os_error(),
        ))),
    };
    if let Err(error) = result {
        if created {
            remove_matching_file(&dir, &name, &file);
        }
        return Err(error);
    }
    Ok(SessionRunLock {
        file,
        dir,
        name,
        remove_on_drop: created,
    })
}

#[cfg(unix)]
fn lock_namespace(dir: &Dir) -> Result<NamespaceLock, StoreError> {
    use rustix::fs::{FlockOperation, flock};

    let (file, _) = open_private_lock_file(dir, LOCK_NAMESPACE_FILE, || Ok(()))?;
    flock(&file, FlockOperation::LockExclusive)
        .map_err(|error| StoreError::Io(io::Error::from_raw_os_error(error.raw_os_error())))?;
    Ok(NamespaceLock(file))
}

#[cfg(unix)]
fn open_private_lock_file(
    dir: &Dir,
    name: &str,
    before_create: impl FnOnce() -> Result<(), StoreError>,
) -> Result<(File, bool), StoreError> {
    use cap_std::fs::{MetadataExt, OpenOptionsExt};

    let mut existing = OpenOptions::new();
    existing.read(true).write(true);
    existing.follow(FollowSymlinks::No);
    nonblocking(&mut existing);
    let (file, created) = match dir.open_with(name, &existing) {
        Ok(file) => (file, false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            before_create()?;
            let mut create = OpenOptions::new();
            create.read(true).write(true).create_new(true).mode(0o600);
            create.follow(FollowSymlinks::No);
            nonblocking(&mut create);
            match dir.open_with(name, &create) {
                Ok(file) => (file, true),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    (dir.open_with(name, &existing)?, false)
                }
                Err(error) => return Err(StoreError::Io(error)),
            }
        }
        Err(error) => return Err(StoreError::Io(error)),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() != 0
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok((file, created))
}

#[cfg(unix)]
fn check_lock_file_quota(dir: &Dir, limit: usize) -> Result<(), StoreError> {
    if limit == 0 {
        return Err(StoreError::StorageFull);
    }
    let mut entries = 0;
    let mut lock_files = 0;
    for entry in dir.entries()? {
        entries += 1;
        if entries > limit.saturating_add(MAX_STATE_OVERHEAD_FILES) {
            return Err(StoreError::StorageFull);
        }
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("session-") && name.ends_with(".lock") {
            lock_files += 1;
            if lock_files >= limit {
                return Err(StoreError::StorageFull);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn remove_matching_file(dir: &Dir, name: &str, file: &File) {
    use cap_std::fs::MetadataExt;

    if let (Ok(current), Ok(held)) = (dir.symlink_metadata(name), file.metadata())
        && current.dev() == held.dev()
        && current.ino() == held.ino()
    {
        let _ = dir.remove_file(name);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn account_replacement_lock_is_private_bounded_and_reusable() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        let other = StateRoot::admit(&path).expect("same private state");
        let lock_path = path.join(ACCOUNT_LOCK_FILE);

        state
            .with_account_replacement_lock(&workspace, || {
                assert!(matches!(
                    other.with_account_replacement_lock(&workspace, || ()),
                    Err(StoreError::AccountBusy)
                ));
            })
            .expect("first account replacement");
        other
            .with_account_replacement_lock(&workspace, || ())
            .expect("reacquire account replacement");
        let metadata = std::fs::metadata(&lock_path).expect("account lock metadata");
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o077, 0);

        let alias = temp.path().join("workspace-alias");
        symlink(&workspace, &alias).expect("workspace alias");
        let nested = StateRoot::admit(&workspace.join("nested-state")).expect("nested state");
        assert!(matches!(
            nested.with_account_replacement_lock(&alias, || ()),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(
            !workspace
                .join("nested-state")
                .join(ACCOUNT_LOCK_FILE)
                .exists()
        );
    }

    #[test]
    fn account_replacement_rejects_unsafe_existing_lock_objects() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        let lock_path = path.join(ACCOUNT_LOCK_FILE);
        let target = temp.path().join("target");
        std::fs::write(&target, "target").expect("target");

        symlink(&target, &lock_path).expect("symlink");
        assert!(
            state
                .with_account_replacement_lock(&workspace, || ())
                .is_err()
        );
        std::fs::remove_file(&lock_path).expect("remove symlink");
        std::fs::hard_link(&target, &lock_path).expect("hard link");
        assert!(matches!(
            state.with_account_replacement_lock(&workspace, || ()),
            Err(StoreError::StateNotPrivate)
        ));
        std::fs::remove_file(&lock_path).expect("remove hard link");
        std::fs::write(&lock_path, "content").expect("nonempty lock");
        assert!(matches!(
            state.with_account_replacement_lock(&workspace, || ()),
            Err(StoreError::StateNotPrivate)
        ));
        std::fs::write(&lock_path, "").expect("empty lock");
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644))
            .expect("public lock");
        assert!(matches!(
            state.with_account_replacement_lock(&workspace, || ()),
            Err(StoreError::StateNotPrivate)
        ));
    }

    #[test]
    fn lock_files_are_private_and_precommit_files_are_removed() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        let session_id = SessionId::new();
        let lock_path = path.join(format!("session-{session_id}.lock"));

        let guard = state.lock_session(session_id).expect("new lock");
        assert!(matches!(
            state.lock_session(session_id),
            Err(StoreError::SessionBusy)
        ));
        drop(guard);
        assert!(!lock_path.exists());

        let mut guard = state.lock_session(session_id).expect("reacquire");
        guard.keep();
        drop(guard);
        assert!(lock_path.exists());
        for _ in 0..128 {
            let guard = state.lock_session(session_id).expect("persistent lock");
            drop(guard);
        }
    }

    #[test]
    fn unsafe_existing_lock_objects_are_rejected() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");

        let symlink_id = SessionId::new();
        let target = temp.path().join("target");
        std::fs::write(&target, "target").expect("target");
        symlink(&target, path.join(format!("session-{symlink_id}.lock"))).expect("symlink");
        assert!(state.lock_session(symlink_id).is_err());

        let directory_id = SessionId::new();
        std::fs::create_dir(path.join(format!("session-{directory_id}.lock"))).expect("directory");
        assert!(state.lock_session(directory_id).is_err());

        let hardlink_id = SessionId::new();
        std::fs::hard_link(&target, path.join(format!("session-{hardlink_id}.lock")))
            .expect("hardlink");
        assert!(matches!(
            state.lock_session(hardlink_id),
            Err(StoreError::StateNotPrivate)
        ));

        let content_id = SessionId::new();
        std::fs::write(path.join(format!("session-{content_id}.lock")), "content")
            .expect("content");
        assert!(matches!(
            state.lock_session(content_id),
            Err(StoreError::StateNotPrivate)
        ));

        let mode_id = SessionId::new();
        let mode_path = path.join(format!("session-{mode_id}.lock"));
        std::fs::write(&mode_path, "").expect("empty lock file");
        std::fs::set_permissions(&mode_path, std::fs::Permissions::from_mode(0o644))
            .expect("public mode");
        assert!(matches!(
            state.lock_session(mode_id),
            Err(StoreError::StateNotPrivate)
        ));
    }

    #[test]
    fn lock_file_quota_bounds_orphans_without_blocking_existing_sessions() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        let first = SessionId::new();
        let second = SessionId::new();
        let rejected = SessionId::new();
        let rejected_path = path.join(format!("session-{rejected}.lock"));

        let mut first_lock = lock_session_with_limit(&state, first, 2).expect("first lock");
        first_lock.keep();
        drop(first_lock);
        std::fs::write(path.join(format!("session-{second}.lock")), "")
            .expect("simulated crash orphan");
        std::fs::set_permissions(
            path.join(format!("session-{second}.lock")),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("private crash orphan");

        assert!(matches!(
            lock_session_with_limit(&state, rejected, 2),
            Err(StoreError::StorageFull)
        ));
        assert!(!rejected_path.exists());
        let existing = lock_session_with_limit(&state, first, 2).expect("existing Session");
        drop(existing);
        let orphan = lock_session_with_limit(&state, second, 2).expect("reusable crash orphan");
        drop(orphan);

        std::fs::remove_file(path.join(format!("session-{second}.lock")))
            .expect("remove simulated orphan");
        let new_lock = lock_session_with_limit(&state, rejected, 2).expect("capacity recovered");
        drop(new_lock);
        assert!(!rejected_path.exists());

        for index in 0..33 {
            std::fs::write(path.join(format!("extra-{index}")), "").expect("unrelated state file");
        }
        assert!(matches!(
            lock_session_with_limit(&state, rejected, 2),
            Err(StoreError::StorageFull)
        ));
        assert!(!rejected_path.exists());
        let existing = lock_session_with_limit(&state, first, 2).expect("existing Session");
        drop(existing);
    }

    #[test]
    fn namespace_lock_is_private_and_rejects_unsafe_replacement() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        let session_id = SessionId::new();
        let guard = state.lock_session(session_id).expect("first lock");
        let namespace_path = path.join(LOCK_NAMESPACE_FILE);
        let metadata = std::fs::metadata(&namespace_path).expect("namespace metadata");
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        drop(guard);

        std::fs::set_permissions(&namespace_path, std::fs::Permissions::from_mode(0o644))
            .expect("public namespace mode");
        assert!(matches!(
            state.lock_session(SessionId::new()),
            Err(StoreError::StateNotPrivate)
        ));
    }
}

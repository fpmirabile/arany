pub mod engine;
pub mod git;
pub mod goals;
#[cfg(unix)]
pub mod process;
pub mod routing;
pub mod scenario;

pub use crate::provider::test_image;

static ACCOUNT_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
#[cfg(unix)]
static ACCOUNT_LEASE: std::sync::OnceLock<cap_std::fs::File> = std::sync::OnceLock::new();

pub fn isolate_account_root() -> &'static std::path::Path {
    ACCOUNT_ROOT
        .get_or_init(|| {
            #[cfg(debug_assertions)]
            if let Some(path) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
                let path = std::path::PathBuf::from(path);
                assert!(path.is_absolute(), "absolute inherited test account root");
                #[cfg(unix)]
                if let Some(lease) = account_roots::inherit(&path).expect("inherited account lease")
                {
                    ACCOUNT_LEASE.set(lease).expect("one account lease");
                }
                return path;
            }
            #[cfg(unix)]
            {
                // macOS `/tmp` is a symlink, which private admission refuses.
                let parent = std::fs::canonicalize(std::env::temp_dir())
                    .expect("host temporary directory")
                    .join(account_roots::parent_name());
                let (path, lease) =
                    account_roots::create_in(&parent, 128).expect("private test account root");
                ACCOUNT_LEASE.set(lease).expect("one account lease");
                path
            }
            #[cfg(not(unix))]
            tempfile::tempdir()
                .expect("private test account root")
                .keep()
        })
        .as_path()
}

pub(crate) fn account_root() -> Option<&'static std::path::Path> {
    ACCOUNT_ROOT.get().map(std::path::PathBuf::as_path)
}

#[cfg(unix)]
mod account_roots {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::{
        Dir, DirBuilder, DirBuilderExt, File, MetadataExt, OpenOptions, OpenOptionsExt,
    };
    use rustix::fs::{FlockOperation, flock};
    use std::{
        io,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    const PREFIX: &str = "root-";
    const MAX_ENTRIES: usize = 256;
    const MAX_PRUNE_ENTRIES: usize = 32_768;
    const MAX_DEPTH: usize = 16;

    pub(super) fn parent_name() -> String {
        format!(
            "arany-test-accounts-{}",
            rustix::process::geteuid().as_raw()
        )
    }

    fn directory(path: &Path, create: bool) -> io::Result<crate::private_fs::PrivateDirectory> {
        crate::private_fs::open_directory(path, create)
            .map_err(|error| io::Error::other(format!("private account fixture: {error:?}")))
    }

    fn lease(dir: &Dir, name: &str) -> io::Result<File> {
        let file = dir.open_with(
            name,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .follow(FollowSymlinks::No),
        )?;
        crate::private_fs::check_acl(&file, true)
            .map_err(|error| io::Error::other(format!("private account lease: {error:?}")))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() != 0
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other("unsafe account fixture lease"));
        }
        Ok(file)
    }

    pub(super) fn inherit(path: &Path) -> io::Result<Option<File>> {
        if !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(PREFIX.as_bytes()))
            || !path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == parent_name().as_str())
        {
            return Ok(None);
        }
        let parent = directory(path.parent().expect("managed root parent"), false)?;
        let _namespace = namespace(&parent.dir)?;
        let root = directory(path, false)?;
        let lease = lease(&root.dir, ".lease")?;
        flock(&lease, FlockOperation::NonBlockingLockShared)?;
        Ok(Some(lease))
    }

    pub(super) fn create_in(parent: &Path, limit: usize) -> io::Result<(PathBuf, File)> {
        let parent = directory(parent, true)?;
        let _namespace = namespace(&parent.dir)?;
        prune(&parent.path, &parent.dir, limit)?;
        let name = format!("{PREFIX}{}", uuid::Uuid::now_v7());
        parent
            .dir
            .create_dir_with(&name, DirBuilder::new().mode(0o700))?;
        let root = parent.dir.open_dir_nofollow(&name)?;
        let lease = lease(&root, ".lease")?;
        flock(&lease, FlockOperation::NonBlockingLockShared)?;
        Ok((parent.path.join(name), lease))
    }

    fn namespace(parent: &Dir) -> io::Result<File> {
        let namespace = lease(parent, ".namespace")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match flock(&namespace, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                    std::thread::yield_now()
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(namespace)
    }

    fn prune(path: &Path, parent: &Dir, limit: usize) -> io::Result<()> {
        let mut retained = 0;
        let mut remaining = MAX_PRUNE_ENTRIES;
        for (index, entry) in parent.entries()?.enumerate() {
            if index >= MAX_ENTRIES {
                return Err(io::Error::other("account fixture entry limit"));
            }
            let entry = entry?;
            let name = entry.file_name();
            if !name.as_encoded_bytes().starts_with(PREFIX.as_bytes()) {
                continue;
            }
            retained += 1;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let root = directory(&path.join(&name), false)?;
            let lease = lease(&root.dir, ".lease")?;
            match flock(&lease, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    remove_contents(&root.dir, &mut remaining, 0)?;
                    parent.remove_dir(&name)?;
                    retained -= 1;
                }
                Err(rustix::io::Errno::WOULDBLOCK) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if retained >= limit {
            return Err(io::Error::other("account fixture root limit"));
        }
        Ok(())
    }

    fn remove_contents(dir: &Dir, remaining: &mut usize, depth: usize) -> io::Result<()> {
        if depth >= MAX_DEPTH {
            return Err(io::Error::other("account fixture depth limit"));
        }
        for entry in dir.entries()? {
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("account fixture pruning limit"))?;
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_dir() {
                let child = dir.open_dir_nofollow(&name)?;
                remove_contents(&child, remaining, depth + 1)?;
                dir.remove_dir(&name)?;
            } else {
                dir.remove_file(&name)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        #[test]
        fn pruning_removes_abandoned_roots_and_preserves_live_roots_and_outside_files() {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().join(parent_name());
            let (live, owner) = create_in(&parent, 4).unwrap();
            let borrower = inherit(&live).unwrap().unwrap();
            drop(owner);
            let (stale, owner) = create_in(&parent, 4).unwrap();
            std::fs::create_dir(stale.join("guard")).unwrap();
            std::fs::write(stale.join("guard/lock"), b"stale").unwrap();
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"outside").unwrap();
            symlink(&outside, stale.join("guard/alias")).unwrap();
            let alias = parent.join("root-alias");
            symlink(&outside, &alias).unwrap();
            drop(owner);
            let (next, owner) = create_in(&parent, 4).unwrap();
            assert!(!stale.exists());
            assert!(live.is_dir() && next.is_dir());
            assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"outside");
            assert!(std::fs::symlink_metadata(&alias).unwrap().is_symlink());
            assert_eq!(
                std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
            drop((borrower, owner));
            let (_last, _owner) = create_in(&parent, 4).unwrap();
            assert!(!live.exists() && !next.exists());
            assert!(outside.join("keep").exists());
            let alias_parent = temp.path().join("alias-parent");
            symlink(&outside, &alias_parent).unwrap();
            assert!(create_in(&alias_parent, 4).is_err());
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        }

        #[test]
        fn root_quota_rejects_live_overflow_and_recovers_after_release() {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().join(parent_name());
            let (first, first_lease) = create_in(&parent, 2).unwrap();
            let (_second, _second_lease) = create_in(&parent, 2).unwrap();
            assert!(create_in(&parent, 2).is_err());
            assert!(first.is_dir());
            drop(first_lease);
            let (_third, _third_lease) = create_in(&parent, 2).unwrap();
            assert!(!first.exists());
        }
    }
}

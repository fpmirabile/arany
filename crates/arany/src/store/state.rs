use super::{DATABASE_FILE, MAX_DATABASE_BYTES, StoreError};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use directories::ProjectDirs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
mod build_cache;
mod lock;
#[cfg(test)]
pub(crate) use build_cache::begin_released;
#[cfg(test)]
pub(crate) use build_cache::capture_archive;
pub(crate) use build_cache::{
    BuildCacheIdentity, BuildCacheLease, DEFAULT_ARCHIVE_BYTES as BUILD_CACHE_ARCHIVE_BYTES,
    archive_size_with_limit,
};
#[cfg(target_os = "macos")]
pub(crate) use build_cache::{capture_archive_async_with_limit, restore_archive_async_with_limit};
#[cfg(target_os = "linux")]
pub(crate) use build_cache::{capture_archive_with_limit, restore_archive_with_limit};
pub(crate) use lock::{SessionRunLock, WorkspaceRunLock};

const HISTORY_FILES: [&str; 4] = [
    "events.sqlite3-journal",
    "events.sqlite3-wal",
    "events.sqlite3-shm",
    DATABASE_FILE,
];

pub struct HistoryReset {
    state: StateRoot,
    _lock: lock::HistoryLock,
    records: [Option<cap_std::fs::Metadata>; 4],
}

impl HistoryReset {
    pub fn apply(self) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;

            let current = self.state.history_records()?;
            for (expected, current) in self.records.iter().zip(&current) {
                match (expected, current) {
                    (None, None) => {}
                    (Some(expected), Some(current))
                        if expected.dev() == current.dev()
                            && expected.ino() == current.ino()
                            && expected.len() == current.len()
                            && expected.modified()? == current.modified()? => {}
                    _ => return Err(StoreError::HistoryChanged),
                }
            }
            for (name, record) in HISTORY_FILES.iter().zip(&self.records) {
                if record.is_some() {
                    self.state.dir.remove_file(name)?;
                }
            }
            prepare_database_file(&self.state.dir, true)?;
            self.state
                .dir
                .open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(StoreError::InvalidStateDirectory)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordKind {
    ToolsConfig,
    WorkspacePermissions,
    ProviderProfiles,
    ProviderRouting,
    Permissions,
    SavedAccount,
    UiPreferences,
    ModelPreferences,
    ChatgptRegistration,
    ChatgptAccounts,
}

impl RecordKind {
    // Kinds without a pending file are read-only to the harness.
    const fn spec(self) -> (&'static str, Option<&'static str>, usize) {
        match self {
            Self::ToolsConfig => ("tools.json", None, 64 * 1024),
            Self::WorkspacePermissions => (
                "workspace-permissions.json",
                Some("workspace-permissions.pending"),
                64 * 1024,
            ),
            Self::ProviderRouting => ("provider-routing.json", None, 16 * 1024),
            Self::Permissions => ("permissions.json", None, 64 * 1024),
            Self::ProviderProfiles => ("provider-profiles.json", None, 64 * 1024),
            Self::SavedAccount => (
                "account-credentials.json",
                Some("account-credentials.pending"),
                1024,
            ),
            Self::UiPreferences => ("ui-preferences.json", Some("ui-preferences.pending"), 1024),
            Self::ModelPreferences => (
                "model-preferences.json",
                Some("model-preferences.pending"),
                1024 * 1024,
            ),
            Self::ChatgptRegistration => (
                "chatgpt-registration.json",
                Some("chatgpt-registration.pending"),
                256,
            ),
            Self::ChatgptAccounts => (
                "chatgpt-accounts.json",
                Some("chatgpt-accounts.pending"),
                384 * 1024,
            ),
        }
    }

    #[cfg(test)]
    const fn file(self) -> &'static str {
        self.spec().0
    }

    #[cfg(test)]
    fn pending(self) -> &'static str {
        self.spec().1.expect("writable record kind")
    }

    pub const fn limit(self) -> usize {
        self.spec().2
    }
}

pub struct Record<'a> {
    root: &'a StateRoot,
    kind: RecordKind,
}

impl Record<'_> {
    pub fn read(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let (name, _, limit) = self.kind.spec();
        self.root.read_private_record(name, limit)
    }

    pub fn present(&self) -> Result<bool, StoreError> {
        let (name, _, limit) = self.kind.spec();
        Ok(self.root.open_private_record(name, limit)?.is_some())
    }

    pub fn replace(&self, record: &[u8]) -> Result<(), StoreError> {
        let (name, pending, limit) = self.kind.spec();
        let pending = pending.ok_or(StoreError::ReadOnly)?;
        self.root
            .replace_private_record(name, pending, record, limit)
    }

    pub fn remove(&self) -> Result<(), StoreError> {
        let (name, pending, limit) = self.kind.spec();
        let pending = pending.ok_or(StoreError::ReadOnly)?;
        self.root.remove_private_record(name, pending, limit)
    }
}

pub struct StateRoot {
    path: PathBuf,
    dir: Dir,
    #[cfg(unix)]
    ancestors: Vec<(u64, u64)>,
}

impl StateRoot {
    pub(crate) fn try_clone(&self) -> Result<Self, StoreError> {
        Ok(Self {
            path: self.path.clone(),
            dir: self.dir.try_clone()?,
            #[cfg(unix)]
            ancestors: self.ancestors.clone(),
        })
    }

    pub fn default_path() -> Result<PathBuf, StoreError> {
        let dirs =
            ProjectDirs::from("dev", "Arany", "arany").ok_or(StoreError::InvalidStateDirectory)?;
        #[cfg(target_os = "linux")]
        let path = dirs.state_dir().ok_or(StoreError::InvalidStateDirectory)?;
        #[cfg(not(target_os = "linux"))]
        let path = dirs.data_local_dir();
        #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
        if let Some(fixture) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
            let fixture = PathBuf::from(fixture);
            let parent = fixture
                .parent()
                .filter(|parent| fixture.is_absolute() && *parent != Path::new("/"))
                .ok_or(StoreError::InvalidStateDirectory)?;
            if !path.starts_with(parent) {
                return Err(StoreError::InvalidStateDirectory);
            }
        }
        Ok(path.to_path_buf())
    }

    pub fn account_path() -> Result<PathBuf, StoreError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use nix::unistd::{User, geteuid};

            #[cfg(debug_assertions)]
            if let Some(path) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(StoreError::InvalidStateDirectory);
                }
                return Ok(path);
            }
            #[cfg(not(debug_assertions))]
            if std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_some() {
                return Err(StoreError::InvalidStateDirectory);
            }

            let user = User::from_uid(geteuid())
                .map_err(|_| StoreError::InvalidStateDirectory)?
                .ok_or(StoreError::InvalidStateDirectory)?;
            if !user.dir.is_absolute() {
                return Err(StoreError::InvalidStateDirectory);
            }
            #[cfg(target_os = "linux")]
            let suffix = ".local/state/arany";
            #[cfg(target_os = "macos")]
            let suffix = "Library/Application Support/dev.Arany.arany";
            Ok(user.dir.join(suffix))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Self::default_path()
        }
    }

    pub fn admit(path: &Path) -> Result<Self, StoreError> {
        Self::open(path, true)
    }

    pub fn open_existing(path: &Path) -> Result<Self, StoreError> {
        Self::open(path, false)
    }

    pub(crate) fn readmit(&self) -> Result<(), StoreError> {
        let current = Self::open_existing(&self.path)?;
        #[cfg(unix)]
        if directory_identity(&current.dir)? == directory_identity(&self.dir)? {
            return Ok(());
        }
        Err(StoreError::InvalidStateDirectory)
    }

    fn open(path: &Path, create: bool) -> Result<Self, StoreError> {
        let directory = crate::private_fs::open_directory(path, create)?;
        #[cfg(unix)]
        if create {
            lock::prepare_history_lock(&directory.dir)?;
        }
        prepare_database_file(&directory.dir, create)?;
        Ok(Self {
            path: directory.path,
            dir: directory.dir,
            #[cfg(unix)]
            ancestors: directory.ancestors,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn prepare_history_reset(self, workspace: &Path) -> Result<HistoryReset, StoreError> {
        let workspace = Dir::open_ambient_dir(workspace, ambient_authority())?;
        self.ensure_outside_workspace(&workspace)?;
        let lock = self.lock_history(true)?;
        let records = self.history_records()?;
        Ok(HistoryReset {
            state: self,
            _lock: lock,
            records,
        })
    }

    fn history_records(&self) -> Result<[Option<cap_std::fs::Metadata>; 4], StoreError> {
        let mut records = std::array::from_fn(|_| None);
        for (name, record) in HISTORY_FILES.iter().zip(&mut records) {
            *record = self
                .open_private_record(name, MAX_DATABASE_BYTES as usize)?
                .map(|file| file.metadata())
                .transpose()?;
        }
        Ok(records)
    }

    pub(crate) fn append_development_diagnostic(&self, record: &[u8]) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            use std::io::{Seek, SeekFrom};
            const LIMIT: usize = 256 * 1024;
            if record.len() > 24 * 1024 {
                return Err(StoreError::StorageFull);
            }
            let mut options = OpenOptions::new();
            options.read(true).write(true).follow(FollowSymlinks::No);
            nonblocking(&mut options);
            let mut file = match self.dir.open_with("development.log", &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    options.create_new(true).mode(0o600);
                    self.dir.open_with("development.log", &options)?
                }
                Err(error) => return Err(error.into()),
            };
            check_private_record_file(&file, LIMIT)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .map_err(|error| StoreError::Io(io::Error::from(error)))?;
            check_private_record_file(&file, LIMIT)?;
            if file.metadata()?.len().saturating_add(record.len() as u64) > LIMIT as u64 {
                file.set_len(0)?;
            }
            file.seek(SeekFrom::End(0))?;
            file.write_all(record)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = record;
            Err(StoreError::InvalidStateDirectory)
        }
    }

    pub fn record(&self, kind: RecordKind) -> Record<'_> {
        Record { root: self, kind }
    }

    fn remove_private_record(
        &self,
        name: &str,
        pending: &str,
        limit: usize,
    ) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            let record = self.open_private_record(name, limit)?;
            let staged = self.open_private_record(pending, limit)?;
            if staged.is_some() {
                self.dir.remove_file(pending)?;
            }
            if record.is_some() {
                self.dir.remove_file(name)?;
            }
            self.dir
                .open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (name, pending, limit);
            Err(StoreError::InvalidStateDirectory)
        }
    }

    fn read_private_record(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(mut file) = self.open_private_record(name, limit)? else {
            return Ok(None);
        };
        let mut record = Vec::new();
        Read::by_ref(&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut record)?;
        if record.len() > limit {
            return Err(StoreError::StateNotPrivate);
        }
        Ok(Some(record))
    }

    fn open_private_record(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<cap_std::fs::File>, StoreError> {
        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true);
            options.follow(FollowSymlinks::No);
            nonblocking(&mut options);
            let file = match self.dir.open_with(name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(StoreError::Io(error)),
            };
            check_private_record_file(&file, limit)?;
            Ok(Some(file))
        }
        #[cfg(not(unix))]
        {
            let _ = (name, limit);
            Err(StoreError::InvalidStateDirectory)
        }
    }

    fn replace_private_record(
        &self,
        name: &str,
        pending: &str,
        record: &[u8],
        limit: usize,
    ) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;

            if record.is_empty() || record.len() > limit {
                return Err(StoreError::StorageFull);
            }
            self.read_private_record(name, limit)?;
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            existing.follow(FollowSymlinks::No);
            nonblocking(&mut existing);
            let mut file = match self.dir.open_with(pending, &existing) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut create = OpenOptions::new();
                    create.read(true).write(true).create_new(true).mode(0o600);
                    create.follow(FollowSymlinks::No);
                    nonblocking(&mut create);
                    self.dir.open_with(pending, &create)?
                }
                Err(error) => return Err(StoreError::Io(error)),
            };
            check_private_record_file(&file, limit)?;
            file.set_len(0)?;
            file.write_all(record)?;
            file.sync_all()?;
            self.dir.rename(pending, &self.dir, name)?;
            self.dir
                .open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (name, pending, record, limit);
            Err(StoreError::InvalidStateDirectory)
        }
    }

    #[cfg(unix)]
    pub(crate) fn ensure_outside_workspace(&self, workspace: &Dir) -> Result<(), StoreError> {
        let identity = directory_identity(workspace)?;
        if self.ancestors.contains(&identity) {
            return Err(StoreError::StateNotPrivate);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn check_private_record_file(file: &cap_std::fs::File, limit: usize) -> Result<(), StoreError> {
    crate::private_fs::check_acl(file, true)?;
    use cap_std::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() > limit as u64
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

#[cfg(unix)]
fn nonblocking(options: &mut OpenOptions) {
    use cap_std::fs::OpenOptionsExt;
    options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
}

#[cfg(unix)]
fn directory_identity(dir: &Dir) -> Result<(u64, u64), StoreError> {
    use cap_std::fs::MetadataExt;
    let metadata = dir.dir_metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn prepare_database_file(dir: &Dir, create: bool) -> Result<(), StoreError> {
    if create {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        options.follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
            nonblocking(&mut options);
        }
        match dir.open_with(DATABASE_FILE, &options) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    let mut existing = OpenOptions::new();
    existing.read(true);
    if create {
        existing.write(true);
    }
    existing.follow(FollowSymlinks::No);
    #[cfg(unix)]
    nonblocking(&mut existing);
    let file = dir.open_with(DATABASE_FILE, &existing)?;
    #[cfg(unix)]
    crate::private_fs::check_acl(&file, true)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_DATABASE_BYTES as u64 {
        return Err(StoreError::StateNotPrivate);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        if meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
        {
            return Err(StoreError::StateNotPrivate);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn readmission_rejects_a_renamed_or_unsafe_state_directory() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private State");
        state.readmit().expect("unchanged State");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("widen State");
        assert!(state.readmit().is_err(), "an unsafe State fails closed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("restore State");
        std::fs::rename(&path, temp.path().join("held")).expect("move held State");
        StateRoot::admit(&path).expect("replacement State");
        assert!(
            matches!(state.readmit(), Err(StoreError::InvalidStateDirectory)),
            "a swapped directory at the State path fails closed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native Linux cross-user account gate; requires subordinate IDs, unshare, bwrap, and setpriv"]
    fn account_root_is_private_across_os_users() {
        const STAGE: &str = "ARANY_TEST_CROSS_USER_STAGE";
        const TEST: &str = "store::state::tests::account_root_is_private_across_os_users";
        const OWNER_ROOT: &str = "/root/.local/state/arany";
        const RECORD: &[u8] = b"synthetic-cross-user-record";

        if let Some(stage) = std::env::var_os(STAGE) {
            match stage.to_str() {
                Some("owner") => {
                    assert_eq!(nix::unistd::geteuid().as_raw(), 0);
                    assert_eq!(StateRoot::account_path().unwrap(), Path::new(OWNER_ROOT));
                    let state =
                        StateRoot::admit(Path::new(OWNER_ROOT)).expect("owner account root");
                    state
                        .record(RecordKind::SavedAccount)
                        .replace(RECORD)
                        .expect("owner account record");
                    assert_eq!(
                        state
                            .record(RecordKind::SavedAccount)
                            .read()
                            .unwrap()
                            .as_deref(),
                        Some(RECORD)
                    );

                    let output = std::process::Command::new("/usr/bin/setpriv")
                        .args([
                            "--reuid",
                            "1",
                            "--regid",
                            "1",
                            "--clear-groups",
                            "/test",
                            "--exact",
                            TEST,
                            "--ignored",
                            "--nocapture",
                        ])
                        .env_clear()
                        .env(STAGE, "other")
                        .output()
                        .expect("second OS user");
                    assert!(
                        output.status.success(),
                        "second OS user failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                            .replace("synthetic-cross-user-record", "[redacted]")
                            .chars()
                            .take(1024)
                            .collect::<String>()
                    );
                    crate::test_process::assert_single_test_completed(
                        &output.stdout,
                        "second OS user",
                    );
                    assert_eq!(
                        state
                            .record(RecordKind::SavedAccount)
                            .read()
                            .unwrap()
                            .as_deref(),
                        Some(RECORD)
                    );
                }
                Some("other") => {
                    assert_eq!(nix::unistd::geteuid().as_raw(), 1);
                    assert_eq!(
                        StateRoot::account_path().unwrap(),
                        Path::new("/other/.local/state/arany")
                    );
                    assert_eq!(
                        std::fs::read(Path::new(OWNER_ROOT).join(RecordKind::SavedAccount.file()))
                            .expect_err("another OS user read the saved record")
                            .kind(),
                        io::ErrorKind::PermissionDenied
                    );
                    assert!(StateRoot::open_existing(Path::new(OWNER_ROOT)).is_err());
                }
                _ => panic!("invalid cross-user test stage"),
            }
            return;
        }

        let temp = tempfile::tempdir().expect("private cross-user fixture");
        let home = temp.path().join("owner-home");
        std::fs::create_dir(&home).expect("private owner home");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
            .expect("owner-only home");
        let passwd = temp.path().join("passwd");
        std::fs::write(
            &passwd,
            b"owner:x:0:0::/root:/bin/sh\nother:x:1:1::/other:/bin/sh\n",
        )
        .expect("synthetic passwd entries");
        let test_exe = std::env::current_exe().expect("test executable");
        let output = std::process::Command::new("/usr/bin/timeout")
            .args([
                "-k",
                "2s",
                "20s",
                "/usr/bin/unshare",
                "--map-auto",
                "--map-user",
                "0",
                "--map-group",
                "0",
                "--user",
                "--mount",
                "--net",
                "--fork",
                "--kill-child",
                "/usr/bin/bwrap",
                "--unshare-pid",
                "--die-with-parent",
                "--tmpfs",
                "/",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/lib",
                "/lib",
                "--symlink",
                "usr/lib",
                "/lib64",
                "--ro-bind",
            ])
            .arg(&passwd)
            .args(["/etc/passwd", "--chmod", "0755", "/etc", "--bind"])
            .arg(&home)
            .args(["/root", "--ro-bind"])
            .arg(test_exe)
            .args([
                "/test",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--chdir",
                "/root",
                "--clearenv",
                "--setenv",
                STAGE,
                "owner",
                "--",
                "/test",
                "--exact",
                TEST,
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .output()
            .expect("isolated cross-user test");
        assert!(
            output.status.success(),
            "isolated cross-user test failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .replace("synthetic-cross-user-record", "[redacted]")
                .chars()
                .take(1024)
                .collect::<String>()
        );
        crate::test_process::assert_single_test_completed(&output.stdout, "cross-user");
    }

    #[test]
    fn read_only_record_kinds_refuse_replace_and_remove() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        for kind in [
            RecordKind::ToolsConfig,
            RecordKind::ProviderProfiles,
            RecordKind::Permissions,
        ] {
            assert!(matches!(
                state.record(kind).replace(b"synthetic"),
                Err(StoreError::ReadOnly)
            ));
            assert!(matches!(
                state.record(kind).remove(),
                Err(StoreError::ReadOnly)
            ));
            assert!(!path.join(kind.file()).exists());
        }
        assert!(std::fs::read_dir(&path).unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_none_or(|e| e != "pending")
        }));
    }

    #[test]
    fn saved_account_file_is_private_atomic_and_refuses_unsafe_objects() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .read()
                .expect("empty state")
                .is_none()
        );
        assert!(
            !state
                .record(RecordKind::SavedAccount)
                .present()
                .expect("empty record")
        );
        assert!(
            !state
                .record(RecordKind::ChatgptAccounts)
                .present()
                .expect("empty index")
        );

        state
            .record(RecordKind::SavedAccount)
            .replace(b"synthetic-first")
            .expect("first account");
        state
            .record(RecordKind::SavedAccount)
            .replace(b"synthetic-second")
            .expect("replacement account");
        assert_eq!(
            state
                .record(RecordKind::SavedAccount)
                .read()
                .expect("saved account"),
            Some(b"synthetic-second".to_vec())
        );
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .present()
                .expect("saved marker")
        );
        assert_eq!(
            std::fs::metadata(path.join(RecordKind::SavedAccount.file()))
                .expect("saved metadata")
                .mode()
                & 0o077,
            0
        );
        assert!(!path.join(RecordKind::SavedAccount.pending()).exists());
        std::fs::write(
            path.join(RecordKind::SavedAccount.pending()),
            b"aborted-replacement",
        )
        .unwrap();
        std::fs::set_permissions(
            path.join(RecordKind::SavedAccount.pending()),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        state.record(RecordKind::SavedAccount).remove().unwrap();
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .read()
                .unwrap()
                .is_none()
        );
        assert!(!path.join(RecordKind::SavedAccount.pending()).exists());
        state.record(RecordKind::SavedAccount).remove().unwrap();
        state
            .record(RecordKind::SavedAccount)
            .replace(b"synthetic-second")
            .unwrap();

        let file = path.join(RecordKind::SavedAccount.file());
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside-stays").expect("outside fixture");
        std::fs::remove_file(&file).expect("remove test record");
        symlink(&outside, &file).expect("symlink fixture");
        assert!(state.record(RecordKind::SavedAccount).remove().is_err());
        assert!(state.record(RecordKind::SavedAccount).present().is_err());
        assert!(state.record(RecordKind::SavedAccount).read().is_err());
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .replace(b"new")
                .is_err()
        );
        assert_eq!(
            std::fs::read(&outside).expect("outside file"),
            b"outside-stays"
        );
        std::fs::remove_file(&file).expect("remove test symlink");

        std::fs::hard_link(&outside, &file).expect("hard-link fixture");
        assert!(state.record(RecordKind::SavedAccount).remove().is_err());
        assert!(state.record(RecordKind::SavedAccount).present().is_err());
        assert!(state.record(RecordKind::SavedAccount).read().is_err());
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .replace(b"new")
                .is_err()
        );
        std::fs::remove_file(&file).expect("remove test hard link");

        symlink(&outside, path.join(RecordKind::SavedAccount.pending()))
            .expect("pending symlink fixture");
        assert!(state.record(RecordKind::SavedAccount).remove().is_err());
        assert!(
            state
                .record(RecordKind::SavedAccount)
                .replace(b"new")
                .is_err()
        );
        assert_eq!(
            std::fs::read(&outside).expect("outside file"),
            b"outside-stays"
        );
    }

    #[test]
    fn chatgpt_records_are_private_atomic_and_refuse_unsafe_objects() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .unwrap()
                .is_none()
        );
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(b"synthetic-first")
            .expect("first registration");
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(b"synthetic-second")
            .expect("replacement registration");
        assert_eq!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .unwrap(),
            Some(b"synthetic-second".to_vec())
        );
        assert_eq!(
            std::fs::metadata(path.join(RecordKind::ChatgptRegistration.file()))
                .unwrap()
                .mode()
                & 0o077,
            0
        );
        assert!(
            !path
                .join(RecordKind::ChatgptRegistration.pending())
                .exists()
        );
        state
            .record(RecordKind::ChatgptRegistration)
            .remove()
            .unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .unwrap()
                .is_none()
        );
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(b"synthetic-second")
            .unwrap();

        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside-stays").unwrap();
        let file = path.join(RecordKind::ChatgptRegistration.file());
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, &file).unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .remove()
                .is_err()
        );
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .is_err()
        );
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(b"new")
                .is_err()
        );
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&outside, &file).unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .remove()
                .is_err()
        );
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .is_err()
        );
        std::fs::remove_file(&file).unwrap();
        symlink(
            &outside,
            path.join(RecordKind::ChatgptRegistration.pending()),
        )
        .unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .remove()
                .is_err()
        );
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(b"new")
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside-stays");
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(&vec![b'x'; 257])
                .is_err()
        );

        assert!(
            state
                .record(RecordKind::ChatgptAccounts)
                .read()
                .unwrap()
                .is_none()
        );
        state
            .record(RecordKind::ChatgptAccounts)
            .replace(b"synthetic-token-index")
            .expect("bounded account index");
        assert!(state.record(RecordKind::ChatgptAccounts).present().unwrap());
        assert_eq!(
            state.record(RecordKind::ChatgptAccounts).read().unwrap(),
            Some(b"synthetic-token-index".to_vec())
        );
        assert_eq!(
            std::fs::metadata(path.join(RecordKind::ChatgptAccounts.file()))
                .unwrap()
                .mode()
                & 0o077,
            0
        );
        assert!(!path.join(RecordKind::ChatgptAccounts.pending()).exists());
        state.record(RecordKind::ChatgptAccounts).remove().unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptAccounts)
                .read()
                .unwrap()
                .is_none()
        );
        state
            .record(RecordKind::ChatgptAccounts)
            .replace(b"synthetic-token-index")
            .unwrap();
        assert!(
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(&vec![b'x'; RecordKind::ChatgptAccounts.limit() + 1])
                .is_err()
        );
        let file = path.join(RecordKind::ChatgptAccounts.file());
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, &file).unwrap();
        assert!(state.record(RecordKind::ChatgptAccounts).remove().is_err());
        assert!(state.record(RecordKind::ChatgptAccounts).present().is_err());
        assert!(state.record(RecordKind::ChatgptAccounts).read().is_err());
        assert!(
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(b"new")
                .is_err()
        );
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&outside, &file).unwrap();
        assert!(state.record(RecordKind::ChatgptAccounts).remove().is_err());
        assert!(state.record(RecordKind::ChatgptAccounts).present().is_err());
        assert!(state.record(RecordKind::ChatgptAccounts).read().is_err());
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, path.join(RecordKind::ChatgptAccounts.pending())).unwrap();
        assert!(state.record(RecordKind::ChatgptAccounts).remove().is_err());
        assert!(
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(b"new")
                .is_err()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"outside-stays");
    }

    #[test]
    fn special_state_files_are_rejected_before_read_or_lock() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&path).expect("private state");

        for name in [
            RecordKind::SavedAccount.file(),
            RecordKind::ChatgptRegistration.file(),
            RecordKind::ChatgptAccounts.file(),
            RecordKind::ProviderProfiles.file(),
            "model-preferences.json",
            "ui-preferences.json",
            "events.sqlite3-journal",
            "events.sqlite3-wal",
            "events.sqlite3-shm",
        ] {
            mkfifo(&path.join(name), Mode::S_IRUSR | Mode::S_IWUSR).expect("test FIFO");
        }
        assert!(state.record(RecordKind::SavedAccount).remove().is_err());
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .remove()
                .is_err()
        );
        assert!(state.record(RecordKind::ChatgptAccounts).remove().is_err());
        assert!(state.record(RecordKind::ModelPreferences).remove().is_err());
        assert!(matches!(
            StateRoot::open_existing(&path)
                .unwrap()
                .prepare_history_reset(&workspace),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::SavedAccount).read(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::SavedAccount).present(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::ChatgptRegistration).read(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::ChatgptAccounts).read(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::ChatgptAccounts).present(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::ProviderProfiles).read(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.record(RecordKind::ModelPreferences).read(),
            Err(StoreError::StateNotPrivate)
        ));

        assert!(matches!(
            state.record(RecordKind::UiPreferences).read(),
            Err(StoreError::StateNotPrivate)
        ));
        std::fs::remove_file(path.join("ui-preferences.json")).unwrap();
        std::fs::remove_file(path.join(RecordKind::SavedAccount.file()))
            .expect("remove account FIFO");
        std::fs::remove_file(path.join("model-preferences.json")).expect("remove preference FIFO");
        for name in [
            RecordKind::SavedAccount.pending(),
            RecordKind::ChatgptRegistration.pending(),
            RecordKind::ChatgptAccounts.pending(),
            "account-credentials.lock",
            "model-preferences.pending",
            "ui-preferences.pending",
        ] {
            mkfifo(&path.join(name), Mode::S_IRUSR | Mode::S_IWUSR).expect("test FIFO");
        }
        assert!(matches!(
            state.record(RecordKind::SavedAccount).replace(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state
                .record(RecordKind::ModelPreferences)
                .replace(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.with_account_replacement_lock(&workspace, || ()),
            Err(StoreError::StateNotPrivate)
        ));

        assert!(matches!(
            state
                .record(RecordKind::UiPreferences)
                .replace(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        drop(state);
        let alias = path.join("database-alias");
        std::fs::hard_link(path.join(DATABASE_FILE), &alias).expect("database hard link");
        for result in [StateRoot::open_existing(&path), StateRoot::admit(&path)] {
            assert!(matches!(result, Err(StoreError::StateNotPrivate)));
        }
        std::fs::remove_file(alias).expect("remove database hard link");
        std::fs::remove_file(path.join(DATABASE_FILE)).expect("remove database");
        mkfifo(&path.join(DATABASE_FILE), Mode::S_IRUSR | Mode::S_IWUSR).expect("test FIFO");
        assert!(matches!(
            StateRoot::open_existing(&path),
            Err(StoreError::StateNotPrivate)
        ));
    }
}

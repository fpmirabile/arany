use super::{DATABASE_FILE, MAX_DATABASE_BYTES, RecordFormat, StoreError};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use directories::ProjectDirs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
mod archive;
#[cfg(test)]
pub(super) use archive::FAIL_AT;
pub(super) use archive::identity_of;
pub use archive::{ArchivedHistory, ArchivedRecords};
use archive::{private_subdirectory, rename, sync};
mod build_cache;
pub(crate) use build_cache::{
    CACHE_ROOTS, CacheChange, CacheGuardRecovery, validate_path as validate_cache_path,
};
pub(crate) mod checkpoints;
mod inventory;
mod lock;
pub(crate) mod spill;
pub(crate) use build_cache::{
    BuildCacheIdentity, BuildCacheLease, DEFAULT_VIEW_BYTES as BUILD_CACHE_DEFAULT_VIEW_BYTES,
};
pub use inventory::{
    StorageCategory, StorageError, StorageReason, StorageReport, StorageStatus, StorageUsage,
    collect_storage,
};
pub(crate) use lock::{MAX_SESSION_LOCK_FILES, MAX_STATE_OVERHEAD_FILES, MAX_WORKSPACE_LOCK_FILES};
pub(crate) use lock::{SessionRunLock, WorkspaceRunLock};

pub(super) const HISTORY_FILES: [&str; 4] = [
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
    }
}

// One list generates both the enum and `ALL`, so no variant can be left out of the format checks.
macro_rules! record_kinds {
    ($($kind:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum RecordKind {
            $($kind),+
        }

        impl RecordKind {
            pub const ALL: &'static [Self] = &[$(Self::$kind),+];
        }
    };
}

record_kinds!(
    ToolsConfig,
    WorkspacePermissions,
    ProviderProfiles,
    ProviderRouting,
    ProviderRoster,
    Permissions,
    UnguardedCommands,
    CommandEnvironment,
    CommandTools,
    SavedAccount,
    UiPreferences,
    ModelPreferences,
    ChatgptRegistration,
    ChatgptAccounts,
    McpOauth,
    UserInstructions,
);

impl RecordKind {
    // User-authored kinds are read-only to the harness; permissions.json changes only through the typed settings edit.
    const fn spec(self) -> (&'static str, &'static str, usize, bool) {
        match self {
            Self::ToolsConfig => ("tools.json", "tools.pending", 64 * 1024, false),
            Self::WorkspacePermissions => (
                "workspace-permissions.json",
                "workspace-permissions.pending",
                64 * 1024,
                true,
            ),
            Self::ProviderRoster => (
                "provider-roster.json",
                "provider-roster.pending",
                64 * 1024,
                false,
            ),
            Self::ProviderRouting => (
                "provider-routing.json",
                "provider-routing.pending",
                16 * 1024,
                false,
            ),
            Self::Permissions => ("permissions.json", "permissions.pending", 64 * 1024, true),
            Self::UnguardedCommands => (
                "unguarded-commands.json",
                "unguarded-commands.pending",
                64 * 1024,
                true,
            ),
            Self::CommandTools => (
                "command-tools.json",
                "command-tools.pending",
                64 * 1024,
                true,
            ),
            Self::CommandEnvironment => (
                "command-environment.json",
                "command-environment.pending",
                64 * 1024,
                true,
            ),
            Self::ProviderProfiles => (
                "provider-profiles.json",
                "provider-profiles.pending",
                64 * 1024,
                false,
            ),
            Self::SavedAccount => (
                "account-credentials.json",
                "account-credentials.pending",
                1024,
                true,
            ),
            Self::UiPreferences => ("ui-preferences.json", "ui-preferences.pending", 1024, true),
            Self::ModelPreferences => (
                "model-preferences.json",
                "model-preferences.pending",
                1024 * 1024,
                true,
            ),
            Self::ChatgptRegistration => (
                "chatgpt-registration.json",
                "chatgpt-registration.pending",
                256,
                true,
            ),
            Self::McpOauth => ("mcp-oauth.json", "mcp-oauth.pending", 64 * 1024, true),
            Self::UserInstructions => ("AGENTS.md", "AGENTS.pending", 64 * 1024, false),
            Self::ChatgptAccounts => (
                "chatgpt-accounts.json",
                "chatgpt-accounts.pending",
                384 * 1024,
                true,
            ),
        }
    }

    #[cfg(test)]
    pub(super) const fn file(self) -> &'static str {
        self.spec().0
    }

    #[cfg(test)]
    pub(super) const fn pending(self) -> &'static str {
        self.spec().1
    }

    pub const fn limit(self) -> usize {
        self.spec().2
    }
}

pub struct Record<'a> {
    root: &'a StateRoot,
    kind: RecordKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRecordTarget {
    Profiles,
    Routing,
    Roster,
}

impl ProviderRecordTarget {
    const fn kind(self) -> RecordKind {
        match self {
            Self::Profiles => RecordKind::ProviderProfiles,
            Self::Routing => RecordKind::ProviderRouting,
            Self::Roster => RecordKind::ProviderRoster,
        }
    }
}

pub enum ProviderRecordEdit<'a> {
    Replace(&'a [u8]),
    Remove,
}

impl Record<'_> {
    pub fn read(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let (name, _, limit, _) = self.kind.spec();
        self.root.read_private_record(name, limit)
    }

    pub fn present(&self) -> Result<bool, StoreError> {
        let (name, _, limit, _) = self.kind.spec();
        Ok(self.root.open_private_record(name, limit)?.is_some())
    }

    pub fn replace(&self, record: &[u8]) -> Result<(), StoreError> {
        if !self.kind.spec().3 {
            return Err(StoreError::ReadOnly);
        }
        let (name, pending, limit, _) = self.kind.spec();
        self.root
            .replace_private_record(name, pending, record, limit)
    }

    pub fn remove(&self) -> Result<(), StoreError> {
        let (name, pending, limit, writable) = self.kind.spec();
        if !writable {
            return Err(StoreError::ReadOnly);
        }
        self.root.remove_private_record(name, pending, limit)
    }
}

pub struct StateRoot {
    path: PathBuf,
    pub(super) dir: Dir,
    ancestors: Vec<(u64, u64)>,
}

#[cfg(all(target_os = "linux", any(test, feature = "test-support")))]
fn passwd_is_mounted() -> bool {
    const MAX_MOUNTINFO_BYTES: u64 = 1024 * 1024;
    let mut mounts = String::new();
    std::fs::File::open("/proc/self/mountinfo")
        .and_then(|file| file.take(MAX_MOUNTINFO_BYTES).read_to_string(&mut mounts))
        .is_ok()
        && mounts
            .lines()
            .any(|mount| mount.split(' ').nth(4) == Some("/etc/passwd"))
}

impl StateRoot {
    pub(crate) fn try_clone(&self) -> Result<Self, StoreError> {
        Ok(Self {
            path: self.path.clone(),
            dir: self.dir.try_clone()?,
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
        if let Some(fixture) = Self::fixture_account_root() {
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
        #[cfg(not(debug_assertions))]
        if std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_some() {
            return Err(StoreError::InvalidStateDirectory);
        }
        if let Some(path) = Self::fixture_account_root() {
            if !path.is_absolute() {
                return Err(StoreError::InvalidStateDirectory);
            }
            return Ok(path);
        }
        Self::configured_account_path()
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn fixture_account_root() -> Option<PathBuf> {
        Self::debug_account_root()
    }

    // Test-support builds include the binary that `cargo test` spawns, so no test process reaches real state.
    #[cfg(any(test, feature = "test-support"))]
    fn fixture_account_root() -> Option<PathBuf> {
        if let Some(path) = crate::test_support::account_root() {
            return Some(path.to_path_buf());
        }
        if let Some(path) = Self::debug_account_root() {
            return Some(path);
        }
        // Linux passwd-home fixtures mount a private passwd file whose home is the fixture's.
        #[cfg(target_os = "linux")]
        if passwd_is_mounted() {
            return None;
        }
        // Optimized builds refuse the fixture variable, so a spawned binary isolates itself.
        if !cfg!(debug_assertions) {
            return Some(crate::test_support::isolate_account_root().to_path_buf());
        }
        panic!(
            "debug test builds never resolve the real account or state root: call \
             arany::test_support::isolate_account_root() or set ARANY_TEST_ACCOUNT_ROOT"
        );
    }

    fn debug_account_root() -> Option<PathBuf> {
        #[cfg(debug_assertions)]
        {
            std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").map(PathBuf::from)
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    fn configured_account_path() -> Result<PathBuf, StoreError> {
        {
            use nix::unistd::{User, geteuid};

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
    }

    pub fn admit(path: &Path) -> Result<Self, StoreError> {
        Self::open(path, true)
    }

    pub fn open_existing(path: &Path) -> Result<Self, StoreError> {
        Self::open(path, false)
    }

    // A missing directory is absent; an existing one without the database is a new root.
    pub fn open_optional(path: &Path) -> Result<Option<Self>, StoreError> {
        match Self::open_existing(path) {
            Ok(root) => Ok(Some(root)),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(path) {
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => Ok(None),
                    Ok(metadata) if metadata.is_dir() => Self::admit(path).map(Some),
                    _ => Err(StoreError::Io(error)),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn readmit(&self) -> Result<(), StoreError> {
        let current = Self::open_existing(&self.path)?;
        if directory_identity(&current.dir)? == directory_identity(&self.dir)? {
            return Ok(());
        }
        Err(StoreError::InvalidStateDirectory)
    }

    fn open(path: &Path, create: bool) -> Result<Self, StoreError> {
        let directory = crate::private_fs::open_directory(path, create)?;
        if create {
            lock::prepare_history_lock(&directory.dir)?;
        }
        prepare_database_file(&directory.dir, create)?;
        Ok(Self {
            path: directory.path,
            dir: directory.dir,
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
        let limit = MAX_DATABASE_BYTES as usize;
        let mut records = std::array::from_fn(|_| None);
        for (name, record) in HISTORY_FILES.iter().zip(&mut records) {
            *record = if *name == DATABASE_FILE {
                let file = self.open_private_record(name, limit)?;
                file.map(|file| file.metadata()).transpose()?
            } else {
                self.history_sidecar(name, limit)?
            };
        }
        Ok(records)
    }

    fn history_sidecar(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<cap_std::fs::Metadata>, StoreError> {
        {
            let Some(file) = self.open_record_file(name)? else {
                return Ok(None);
            };
            crate::private_fs::check_acl(&file, true)?;
            let metadata = file.metadata()?;
            crate::fault::private_record_admission(&self.path, name);
            sidecar_metadata(metadata, limit)
        }
    }

    pub(crate) fn append_development_diagnostic(&self, record: &[u8]) -> Result<(), StoreError> {
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
    }

    pub fn record(&self, kind: RecordKind) -> Record<'_> {
        Record { root: self, kind }
    }

    pub fn edit_provider_record(
        &self,
        target: ProviderRecordTarget,
        expected: Option<&[u8]>,
        edit: ProviderRecordEdit<'_>,
        format: &RecordFormat,
    ) -> Result<(), StoreError> {
        let kind = target.kind();
        if format.kind != kind {
            return Err(StoreError::ReadOnly);
        }
        let record = self.record(kind);
        let (name, pending, limit, _) = kind.spec();
        self.with_record_upgrade_lock(|| {
            if record.read()?.as_deref() != expected {
                return Err(StoreError::RecordChanged);
            }
            match edit {
                ProviderRecordEdit::Replace(bytes) => {
                    if !format.accepts_current(bytes) || bytes.is_empty() || bytes.len() > limit {
                        return Err(StoreError::InvalidHistory);
                    }
                    self.replace_private_record(name, pending, bytes, limit)
                }
                ProviderRecordEdit::Remove => self.remove_private_record(name, pending, limit),
            }
        })
    }

    fn remove_private_record(
        &self,
        name: &str,
        pending: &str,
        limit: usize,
    ) -> Result<(), StoreError> {
        {
            crate::fault::record_publication(&self.path, name)?;
            let record = self.open_private_record(name, limit)?;
            let staged = self.open_private_record(pending, limit)?;
            if staged.is_some() {
                self.dir.remove_file(pending)?;
            }
            if record.is_some() {
                self.dir.remove_file(name)?;
            }
            // The unlink may already be durable, so a failed or faulted sync is unconfirmed, not unchanged.
            let unconfirmed = |_| StoreError::ReplaceUnconfirmed;
            crate::fault::record_sync(&self.path, name)?;
            self.dir
                .open_with(".", OpenOptions::new().read(true))
                .map_err(unconfirmed)?
                .sync_all()
                .map_err(unconfirmed)?;
            Ok(())
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

    // Reads at most `limit + 1` bytes so the caller can name an oversized file.
    pub(crate) fn read_user_file(
        &self,
        directory: &str,
        name: &str,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        {
            let unsafe_link = |error: io::Error| match error.raw_os_error() {
                Some(code) if code == nix::libc::ELOOP || code == nix::libc::ENOTDIR => {
                    StoreError::StateNotPrivate
                }
                _ => StoreError::Io(error),
            };
            let dir = match self.dir.open_dir_nofollow(directory) {
                Ok(dir) => dir,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(unsafe_link(error)),
            };
            crate::private_fs::check_directory(&dir, true)?;
            let mut options = OpenOptions::new();
            options.read(true);
            options.follow(FollowSymlinks::No);
            nonblocking(&mut options);
            let mut file = match dir.open_with(name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(unsafe_link(error)),
            };
            crate::private_fs::check_acl(&file, true)?;
            // Size is the caller's refusal, so only ownership, mode and kind are checked here.
            check_private_metadata(&file.metadata()?, usize::MAX)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
    }

    fn open_private_record(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<cap_std::fs::File>, StoreError> {
        {
            let Some(file) = self.open_record_file(name)? else {
                return Ok(None);
            };
            crate::fault::private_record_admission(&self.path, name);
            check_private_record_file(&file, limit)?;
            Ok(Some(file))
        }
    }

    fn open_record_file(&self, name: &str) -> Result<Option<cap_std::fs::File>, StoreError> {
        let mut options = OpenOptions::new();
        options.read(true);
        options.follow(FollowSymlinks::No);
        nonblocking(&mut options);
        match self.dir.open_with(name, &options) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    fn replace_private_record(
        &self,
        name: &str,
        pending: &str,
        record: &[u8],
        limit: usize,
    ) -> Result<(), StoreError> {
        {
            use cap_std::fs::OpenOptionsExt;

            if record.is_empty() || record.len() > limit {
                return Err(StoreError::StorageFull);
            }
            self.read_private_record(name, limit)?;
            crate::fault::record_publication(&self.path, name)?;
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
            rename(&self.dir, pending, &self.dir, name)?;
            crate::fault::record_sync(&self.path, name)?;
            sync(&self.dir).map_err(|_| StoreError::ReplaceUnconfirmed)
        }
    }

    pub(crate) fn ensure_outside_workspace(&self, workspace: &Dir) -> Result<(), StoreError> {
        let identity = directory_identity(workspace)?;
        if self.ancestors.contains(&identity) {
            return Err(StoreError::StateNotPrivate);
        }
        Ok(())
    }
}

fn sidecar_metadata(
    metadata: cap_std::fs::Metadata,
    limit: usize,
) -> Result<Option<cap_std::fs::Metadata>, StoreError> {
    use cap_std::fs::MetadataExt;
    // A concurrent SQLite commit unlinks its journal; an inode unlinked after open is absent.
    if metadata.nlink() == 0 {
        return Ok(None);
    }
    check_private_metadata(&metadata, limit)?;
    Ok(Some(metadata))
}

pub(super) fn check_private_record_file(
    file: &cap_std::fs::File,
    limit: usize,
) -> Result<(), StoreError> {
    crate::private_fs::check_acl(file, true)?;
    check_private_metadata(&file.metadata()?, limit)
}

fn check_private_metadata(
    metadata: &cap_std::fs::Metadata,
    limit: usize,
) -> Result<(), StoreError> {
    use cap_std::fs::MetadataExt;
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

pub(super) fn nonblocking(options: &mut OpenOptions) {
    use cap_std::fs::OpenOptionsExt;
    options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
}

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
    nonblocking(&mut existing);
    let file = dir.open_with(DATABASE_FILE, &existing)?;
    crate::private_fs::check_acl(&file, true)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_DATABASE_BYTES as u64 {
        return Err(StoreError::StateNotPrivate);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn account_path_uses_the_private_binary_fixture() {
        let path = StateRoot::account_path().expect("isolated account path");
        assert_eq!(path, crate::test_support::isolate_account_root());
        if let Some(inherited) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
            assert_eq!(path, PathBuf::from(inherited));
        } else {
            assert!(path.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
            assert_ne!(path, StateRoot::configured_account_path().unwrap());
        }
        let metadata = std::fs::metadata(&path).expect("test account root");
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
    }

    #[test]
    fn optional_open_skips_only_a_missing_directory_and_admits_an_empty_one() {
        let temp = tempfile::tempdir().expect("temporary root");
        let missing = temp.path().join("missing/state");
        assert!(StateRoot::open_optional(&missing).unwrap().is_none());
        assert!(!temp.path().join("missing").exists());
        let empty = temp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(empty.join("ui-preferences.json"), b"{}").unwrap();
        std::fs::set_permissions(
            empty.join("ui-preferences.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let root = StateRoot::open_optional(&empty)
            .unwrap()
            .expect("admitted root");
        assert!(empty.join(DATABASE_FILE).is_file());
        assert_eq!(
            root.record(crate::store::RecordKind::UiPreferences)
                .read()
                .unwrap(),
            Some(b"{}".to_vec())
        );
        let file = temp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(StateRoot::open_optional(&file).is_err());
    }

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

    // Holds `operation` at the record's admission gate and applies `change` in flight.
    fn with_held_admission<T: Send>(
        state: &StateRoot,
        record: &'static str,
        operation: impl FnOnce() -> T + Send,
        change: impl FnOnce(),
    ) -> T {
        use crate::fault::{FaultPlan, PRIVATE_RECORD_GATE_DEADLINE};
        // A finished or panicking worker cancels an unreached gate, disconnecting its waiter.
        struct Cancel<'a>(&'a StateRoot, &'static str);
        impl Drop for Cancel<'_> {
            fn drop(&mut self) {
                FaultPlan::clear_private_record_admission(self.0.path(), self.1);
            }
        }
        let (entered, release) =
            FaultPlan::gate_private_record_admission(state.path().to_path_buf(), record);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let _cancel = Cancel(state, record);
                operation()
            });
            let held = entered.recv_timeout(PRIVATE_RECORD_GATE_DEADLINE);
            if held.is_ok() {
                change();
                // A worker whose gate already expired reports its failure at the join.
                let _ = release.send(());
            }
            // The closure owns the release, so a panicking change still resumes the worker.
            drop(release);
            let result = worker.join().expect("admission worker");
            held.expect("admission reached its gate");
            result
        })
    }

    #[test]
    fn shared_history_admission_survives_a_journal_unlinked_in_flight() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private State");
        let name = HISTORY_FILES[0];
        let journal = path.join(name);
        let write_journal = || {
            std::fs::write(&journal, b"journal").expect("rollback journal");
            std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600))
                .expect("private journal");
        };
        write_journal();
        // A commit unlinks the journal after admission's metadata snapshot and before validation.
        let admitted = with_held_admission(
            &state,
            name,
            || state.lock_history(false).map(drop),
            || std::fs::remove_file(&journal).expect("commit unlinks the journal"),
        );
        admitted.expect("shared admission");

        write_journal();
        let limit = MAX_DATABASE_BYTES as usize;
        let file = state
            .open_record_file(name)
            .unwrap()
            .expect("open the journal");
        std::fs::remove_file(&journal).expect("commit unlinks the journal");
        let unlinked = file.metadata().expect("unlinked journal metadata");
        assert!(
            sidecar_metadata(unlinked, limit).unwrap().is_none(),
            "a journal unlinked before the snapshot is absent, not unsafe"
        );

        write_journal();
        std::fs::hard_link(&journal, path.join("journal-alias")).expect("journal hard link");
        assert!(matches!(
            state.lock_history(false),
            Err(StoreError::StateNotPrivate)
        ));
    }

    #[test]
    fn a_configuration_record_replaced_during_admission_fails_closed() {
        let temp = tempfile::tempdir().expect("temporary root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private State");
        let kind = RecordKind::Permissions;
        let record = path.join(kind.file());
        let pending = path.join("permissions.edit");
        for (file, bytes) in [(&record, "{}"), (&pending, r#"{"v":2}"#)] {
            std::fs::write(file, bytes).expect("permissions record");
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))
                .expect("private permissions record");
        }
        // An atomic replacement unlinks the opened inode before its privacy validation.
        let read = with_held_admission(
            &state,
            kind.file(),
            || state.record(kind).read(),
            || std::fs::rename(&pending, &record).expect("atomic replacement"),
        );
        assert!(
            matches!(read, Err(StoreError::StateNotPrivate)),
            "a replaced configuration record is never missing: {read:?}"
        );
        assert_eq!(
            state.record(kind).read().unwrap().as_deref(),
            Some(&br#"{"v":2}"#[..])
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
                    assert_eq!(
                        StateRoot::configured_account_path().unwrap(),
                        Path::new(OWNER_ROOT)
                    );
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

                    std::fs::write(state.path.join(RecordKind::ProviderRoster.file()), RECORD)
                        .unwrap();
                    std::fs::set_permissions(
                        state.path.join(RecordKind::ProviderRoster.file()),
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
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
                        StateRoot::configured_account_path().unwrap(),
                        Path::new("/other/.local/state/arany")
                    );
                    assert_eq!(
                        std::fs::read(Path::new(OWNER_ROOT).join(RecordKind::SavedAccount.file()))
                            .expect_err("another OS user read the saved record")
                            .kind(),
                        io::ErrorKind::PermissionDenied
                    );
                    assert_eq!(
                        std::fs::read(
                            Path::new(OWNER_ROOT).join(RecordKind::ProviderRoster.file())
                        )
                        .unwrap_err()
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
            RecordKind::ProviderRouting,
            RecordKind::ProviderRoster,
            RecordKind::UserInstructions,
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
    fn typed_provider_edit_rechecks_snapshot_and_keeps_ordinary_writes_read_only() {
        let temp = tempfile::tempdir().expect("test root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state");
        let format = &crate::provider::PROVIDER_PROFILES;
        let empty = format!(r#"{{"version":{},"profiles":[]}}"#, format.current);
        let empty = empty.as_bytes();
        state
            .edit_provider_record(
                ProviderRecordTarget::Profiles,
                None,
                ProviderRecordEdit::Replace(empty),
                format,
            )
            .expect("write validated provider profiles");
        assert!(matches!(
            state.edit_provider_record(
                ProviderRecordTarget::Profiles,
                None,
                ProviderRecordEdit::Remove,
                format,
            ),
            Err(StoreError::RecordChanged)
        ));
        assert_eq!(
            state.record(RecordKind::ProviderProfiles).read().unwrap(),
            Some(empty.to_vec())
        );
    }

    #[test]
    fn a_record_removal_whose_directory_sync_fails_is_unconfirmed_not_unchanged() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        state
            .record(RecordKind::Permissions)
            .replace(br#"{"permissions":{}}"#)
            .unwrap();
        crate::fault::FaultPlan::fail_record_sync(path.clone(), "permissions.json");
        assert!(matches!(
            state.record(RecordKind::Permissions).remove(),
            Err(StoreError::ReplaceUnconfirmed)
        ));
        assert!(!path.join("permissions.json").exists());
        state.record(RecordKind::Permissions).remove().unwrap();
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

#[cfg(test)]
mod roster_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn provider_roster_records_refuse_unsafe_objects_and_keep_the_opened_root() {
        for kind in [RecordKind::ProviderRoster, RecordKind::UserInstructions] {
            refuses_unsafe_objects_and_keeps_the_opened_root(kind);
        }
    }

    fn refuses_unsafe_objects_and_keeps_the_opened_root(kind: RecordKind) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let root = StateRoot::admit(&path).unwrap();
        let record = root.record(kind);
        let file = path.join(kind.file());
        assert!(record.read().unwrap().is_none());
        let target = temp.path().join("target");
        std::fs::write(&target, b"private canary").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &file).unwrap();
        assert!(record.read().is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&target, &file).unwrap();
        assert!(record.read().is_err());
        std::fs::remove_file(&file).unwrap();
        nix::unistd::mkfifo(
            &file,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(record.read().is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(record.read().is_err());
        std::fs::remove_dir(&file).unwrap();
        for (size, private, accepted) in [
            (64 * 1024, true, true),
            (64 * 1024 + 1, true, false),
            (1, false, false),
        ] {
            std::fs::write(&file, vec![b' '; size]).unwrap();
            std::fs::set_permissions(
                &file,
                std::fs::Permissions::from_mode(if private { 0o600 } else { 0o644 }),
            )
            .unwrap();
            assert_eq!(record.read().is_ok(), accepted);
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"private canary");
        std::fs::write(&file, b"original").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&path, temp.path().join("old-state")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(path.join(kind.file()), b"replacement").unwrap();
        assert!(root.readmit().is_err());
        assert_eq!(record.read().unwrap().unwrap(), b"original");
    }
}

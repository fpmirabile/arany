use super::{DATABASE_FILE, MAX_DATABASE_BYTES, StoreError};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use directories::ProjectDirs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

mod lock;
pub(crate) use lock::SessionRunLock;

const PROVIDER_PROFILES_FILE: &str = "provider-profiles.json";
const MAX_PROVIDER_PROFILES_BYTES: u64 = 64 * 1024;
const SAVED_ACCOUNT_FILE: &str = "account-credentials.json";
const PENDING_ACCOUNT_FILE: &str = "account-credentials.pending";
const CHATGPT_REGISTRATION_FILE: &str = "chatgpt-registration.json";
const PENDING_CHATGPT_REGISTRATION_FILE: &str = "chatgpt-registration.pending";
const CHATGPT_ACCOUNTS_FILE: &str = "chatgpt-accounts.json";
const PENDING_CHATGPT_ACCOUNTS_FILE: &str = "chatgpt-accounts.pending";

pub struct StateRoot {
    path: PathBuf,
    _dir: Dir,
    #[cfg(unix)]
    ancestors: Vec<(u64, u64)>,
}

impl StateRoot {
    pub const MAX_ACCOUNT_RECORD_BYTES: usize = 1024;
    pub const MAX_CHATGPT_REGISTRATION_BYTES: usize = 256;
    pub const MAX_CHATGPT_ACCOUNTS_BYTES: usize = 384 * 1024;
    pub const MAX_MODEL_PREFERENCES_BYTES: usize = 1024 * 1024;

    pub(crate) fn try_clone(&self) -> Result<Self, StoreError> {
        Ok(Self {
            path: self.path.clone(),
            _dir: self._dir.try_clone()?,
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
        Ok(path.to_path_buf())
    }

    pub fn account_path() -> Result<PathBuf, StoreError> {
        #[cfg(target_os = "linux")]
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
            Ok(user.dir.join(".local/state/arany"))
        }
        #[cfg(not(target_os = "linux"))]
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

    fn open(path: &Path, create: bool) -> Result<Self, StoreError> {
        if !path.is_absolute() {
            return Err(StoreError::InvalidStateDirectory);
        }
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(StoreError::InvalidStateDirectory);
        }
        let mut names = Vec::new();
        let mut normalized = PathBuf::from("/");
        for component in components {
            let Component::Normal(name) = component else {
                return Err(StoreError::InvalidStateDirectory);
            };
            names.push(name);
            normalized.push(name);
        }
        if names.is_empty() || normalized != path {
            return Err(StoreError::InvalidStateDirectory);
        }
        let mut dir = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
        #[cfg(unix)]
        let mut ancestors = vec![directory_identity(&dir)?];
        for (index, name) in names.iter().enumerate() {
            match dir.open_dir_nofollow(name) {
                Ok(next) => dir = next,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    let mut builder = DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use cap_std::fs::DirBuilderExt;
                        builder.mode(0o700);
                    }
                    dir.create_dir_with(name, &builder)?;
                    dir = dir.open_dir_nofollow(name)?;
                }
                Err(error) => return Err(StoreError::Io(error)),
            }
            check_directory(&dir, index + 1 == names.len())?;
            #[cfg(unix)]
            ancestors.push(directory_identity(&dir)?);
        }
        prepare_database_file(&dir, create)?;
        Ok(Self {
            path: normalized,
            _dir: dir,
            #[cfg(unix)]
            ancestors,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
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
            let mut file = match self._dir.open_with("development.log", &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    options.create_new(true).mode(0o600);
                    self._dir.open_with("development.log", &options)?
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

    pub(crate) fn read_tools_config(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record("tools.json", 64 * 1024)
    }

    pub(crate) fn read_workspace_permissions(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record("workspace-permissions.json", 64 * 1024)
    }

    pub(crate) fn replace_workspace_permissions(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            "workspace-permissions.json",
            "workspace-permissions.pending",
            record,
            64 * 1024,
        )
    }

    pub(crate) fn read_provider_profiles(&self) -> Result<Vec<u8>, StoreError> {
        let mut options = OpenOptions::new();
        options.read(true);
        options.follow(FollowSymlinks::No);
        #[cfg(unix)]
        nonblocking(&mut options);
        let file = self._dir.open_with(PROVIDER_PROFILES_FILE, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_PROVIDER_PROFILES_BYTES {
            return Err(StoreError::StateNotPrivate);
        }
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            if metadata.nlink() != 1
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o077 != 0
            {
                return Err(StoreError::StateNotPrivate);
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_PROVIDER_PROFILES_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PROVIDER_PROFILES_BYTES {
            return Err(StoreError::StateNotPrivate);
        }
        Ok(bytes)
    }

    pub fn read_saved_account_record(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record(SAVED_ACCOUNT_FILE, Self::MAX_ACCOUNT_RECORD_BYTES)
    }

    pub fn read_ui_preferences_record(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record("ui-preferences.json", 1024)
    }

    pub fn replace_ui_preferences_record(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            "ui-preferences.json",
            "ui-preferences.pending",
            record,
            1024,
        )
    }

    pub fn read_model_preferences_record(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record("model-preferences.json", Self::MAX_MODEL_PREFERENCES_BYTES)
    }

    pub fn replace_model_preferences_record(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            "model-preferences.json",
            "model-preferences.pending",
            record,
            Self::MAX_MODEL_PREFERENCES_BYTES,
        )
    }

    pub fn saved_account_record_present(&self) -> Result<bool, StoreError> {
        Ok(self
            .open_private_record(SAVED_ACCOUNT_FILE, Self::MAX_ACCOUNT_RECORD_BYTES)?
            .is_some())
    }

    pub fn replace_saved_account_record(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            SAVED_ACCOUNT_FILE,
            PENDING_ACCOUNT_FILE,
            record,
            Self::MAX_ACCOUNT_RECORD_BYTES,
        )
    }

    pub fn read_chatgpt_registration_record(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record(
            CHATGPT_REGISTRATION_FILE,
            Self::MAX_CHATGPT_REGISTRATION_BYTES,
        )
    }

    pub fn replace_chatgpt_registration_record(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            CHATGPT_REGISTRATION_FILE,
            PENDING_CHATGPT_REGISTRATION_FILE,
            record,
            Self::MAX_CHATGPT_REGISTRATION_BYTES,
        )
    }

    pub fn read_chatgpt_accounts_record(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_private_record(CHATGPT_ACCOUNTS_FILE, Self::MAX_CHATGPT_ACCOUNTS_BYTES)
    }

    pub fn chatgpt_accounts_record_present(&self) -> Result<bool, StoreError> {
        Ok(self
            .open_private_record(CHATGPT_ACCOUNTS_FILE, Self::MAX_CHATGPT_ACCOUNTS_BYTES)?
            .is_some())
    }

    pub fn replace_chatgpt_accounts_record(&self, record: &[u8]) -> Result<(), StoreError> {
        self.replace_private_record(
            CHATGPT_ACCOUNTS_FILE,
            PENDING_CHATGPT_ACCOUNTS_FILE,
            record,
            Self::MAX_CHATGPT_ACCOUNTS_BYTES,
        )
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
            let file = match self._dir.open_with(name, &options) {
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
            let mut file = match self._dir.open_with(pending, &existing) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut create = OpenOptions::new();
                    create.read(true).write(true).create_new(true).mode(0o600);
                    create.follow(FollowSymlinks::No);
                    nonblocking(&mut create);
                    self._dir.open_with(pending, &create)?
                }
                Err(error) => return Err(StoreError::Io(error)),
            };
            check_private_record_file(&file, limit)?;
            file.set_len(0)?;
            file.write_all(record)?;
            file.sync_all()?;
            self._dir.rename(pending, &self._dir, name)?;
            self._dir
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

#[cfg(unix)]
fn check_directory(dir: &Dir, leaf: bool) -> Result<(), StoreError> {
    use cap_std::fs::MetadataExt;
    let meta = dir.dir_metadata()?;
    let mode = meta.mode();
    let current_uid = rustix::process::geteuid().as_raw();
    if !meta.is_dir() || (meta.uid() != current_uid && meta.uid() != 0) {
        return Err(StoreError::StateNotPrivate);
    }
    if leaf {
        if meta.uid() != current_uid || mode & 0o077 != 0 {
            return Err(StoreError::StateNotPrivate);
        }
    } else if mode & 0o022 != 0 && !(meta.uid() == 0 && mode & 0o1000 != 0) {
        return Err(StoreError::StateNotPrivate);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_directory(_dir: &Dir, _leaf: bool) -> Result<(), StoreError> {
    Err(StoreError::InvalidStateDirectory)
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
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_DATABASE_BYTES as u64 {
        return Err(StoreError::StateNotPrivate);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(StoreError::StateNotPrivate);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rustix::fs::{CWD, Mode, mkfifoat};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[cfg(target_os = "linux")]
    #[test]
    fn account_path_ignores_session_state_environment() {
        use nix::unistd::{User, geteuid};

        const EXPECTED: &str = "ARANY_TEST_EXPECTED_ACCOUNT_PATH";
        #[cfg(not(debug_assertions))]
        const REJECT_OVERRIDE: &str = "ARANY_TEST_REJECT_ACCOUNT_OVERRIDE";
        #[cfg(not(debug_assertions))]
        if std::env::var_os(REJECT_OVERRIDE).is_some() {
            assert!(matches!(
                StateRoot::account_path(),
                Err(StoreError::InvalidStateDirectory)
            ));
            return;
        }
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert_eq!(
                StateRoot::account_path().expect("OS user account path"),
                PathBuf::from(expected)
            );
            return;
        }
        let expected = User::from_uid(geteuid())
            .expect("OS user lookup")
            .expect("OS user")
            .dir
            .join(".local/state/arany");
        let temp = tempfile::tempdir().expect("temporary environment root");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "store::state::tests::account_path_ignores_session_state_environment",
                "--nocapture",
            ])
            .env(EXPECTED, expected)
            .env_remove("ARANY_TEST_ACCOUNT_ROOT")
            .env("HOME", temp.path().join("different-home"))
            .env("XDG_STATE_HOME", temp.path().join("different-state"))
            .output()
            .expect("isolated child");
        assert!(
            output.status.success(),
            "account path changed with the Session environment"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "account-path child did not run its test"
        );
        #[cfg(not(debug_assertions))]
        {
            let output =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args([
                        "--exact",
                        "store::state::tests::account_path_ignores_session_state_environment",
                        "--nocapture",
                    ])
                    .env(REJECT_OVERRIDE, "1")
                    .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
                    .output()
                    .expect("isolated release child");
            assert!(
                output.status.success(),
                "release account override was accepted"
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "release override child did not run its test"
            );
        }
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
                        .replace_saved_account_record(RECORD)
                        .expect("owner account record");
                    assert_eq!(
                        state.read_saved_account_record().unwrap().as_deref(),
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
                    assert!(
                        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                        "second OS user did not exercise the account boundary"
                    );
                    assert_eq!(
                        state.read_saved_account_record().unwrap().as_deref(),
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
                        std::fs::read(Path::new(OWNER_ROOT).join(SAVED_ACCOUNT_FILE))
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
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "cross-user child did not run its test"
        );
    }

    #[test]
    fn saved_account_file_is_private_atomic_and_refuses_unsafe_objects() {
        let temp = tempfile::tempdir().expect("test root");
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).expect("private state");
        assert!(
            state
                .read_saved_account_record()
                .expect("empty state")
                .is_none()
        );
        assert!(!state.saved_account_record_present().expect("empty record"));
        assert!(
            !state
                .chatgpt_accounts_record_present()
                .expect("empty index")
        );

        state
            .replace_saved_account_record(b"synthetic-first")
            .expect("first account");
        state
            .replace_saved_account_record(b"synthetic-second")
            .expect("replacement account");
        assert_eq!(
            state.read_saved_account_record().expect("saved account"),
            Some(b"synthetic-second".to_vec())
        );
        assert!(state.saved_account_record_present().expect("saved marker"));
        assert_eq!(
            std::fs::metadata(path.join(SAVED_ACCOUNT_FILE))
                .expect("saved metadata")
                .mode()
                & 0o077,
            0
        );
        assert!(!path.join(PENDING_ACCOUNT_FILE).exists());

        let file = path.join(SAVED_ACCOUNT_FILE);
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside-stays").expect("outside fixture");
        std::fs::remove_file(&file).expect("remove test record");
        symlink(&outside, &file).expect("symlink fixture");
        assert!(state.saved_account_record_present().is_err());
        assert!(state.read_saved_account_record().is_err());
        assert!(state.replace_saved_account_record(b"new").is_err());
        assert_eq!(
            std::fs::read(&outside).expect("outside file"),
            b"outside-stays"
        );
        std::fs::remove_file(&file).expect("remove test symlink");

        std::fs::hard_link(&outside, &file).expect("hard-link fixture");
        assert!(state.saved_account_record_present().is_err());
        assert!(state.read_saved_account_record().is_err());
        assert!(state.replace_saved_account_record(b"new").is_err());
        std::fs::remove_file(&file).expect("remove test hard link");

        symlink(&outside, path.join(PENDING_ACCOUNT_FILE)).expect("pending symlink fixture");
        assert!(state.replace_saved_account_record(b"new").is_err());
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
        assert!(state.read_chatgpt_registration_record().unwrap().is_none());
        state
            .replace_chatgpt_registration_record(b"synthetic-first")
            .expect("first registration");
        state
            .replace_chatgpt_registration_record(b"synthetic-second")
            .expect("replacement registration");
        assert_eq!(
            state.read_chatgpt_registration_record().unwrap(),
            Some(b"synthetic-second".to_vec())
        );
        assert_eq!(
            std::fs::metadata(path.join(CHATGPT_REGISTRATION_FILE))
                .unwrap()
                .mode()
                & 0o077,
            0
        );
        assert!(!path.join(PENDING_CHATGPT_REGISTRATION_FILE).exists());

        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside-stays").unwrap();
        let file = path.join(CHATGPT_REGISTRATION_FILE);
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, &file).unwrap();
        assert!(state.read_chatgpt_registration_record().is_err());
        assert!(state.replace_chatgpt_registration_record(b"new").is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&outside, &file).unwrap();
        assert!(state.read_chatgpt_registration_record().is_err());
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, path.join(PENDING_CHATGPT_REGISTRATION_FILE)).unwrap();
        assert!(state.replace_chatgpt_registration_record(b"new").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside-stays");
        assert!(
            state
                .replace_chatgpt_registration_record(&vec![b'x'; 257])
                .is_err()
        );

        assert!(state.read_chatgpt_accounts_record().unwrap().is_none());
        state
            .replace_chatgpt_accounts_record(b"synthetic-token-index")
            .expect("bounded account index");
        assert!(state.chatgpt_accounts_record_present().unwrap());
        assert_eq!(
            state.read_chatgpt_accounts_record().unwrap(),
            Some(b"synthetic-token-index".to_vec())
        );
        assert_eq!(
            std::fs::metadata(path.join(CHATGPT_ACCOUNTS_FILE))
                .unwrap()
                .mode()
                & 0o077,
            0
        );
        assert!(!path.join(PENDING_CHATGPT_ACCOUNTS_FILE).exists());
        assert!(
            state
                .replace_chatgpt_accounts_record(&vec![
                    b'x';
                    StateRoot::MAX_CHATGPT_ACCOUNTS_BYTES + 1
                ])
                .is_err()
        );
        let file = path.join(CHATGPT_ACCOUNTS_FILE);
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, &file).unwrap();
        assert!(state.chatgpt_accounts_record_present().is_err());
        assert!(state.read_chatgpt_accounts_record().is_err());
        assert!(state.replace_chatgpt_accounts_record(b"new").is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&outside, &file).unwrap();
        assert!(state.chatgpt_accounts_record_present().is_err());
        assert!(state.read_chatgpt_accounts_record().is_err());
        std::fs::remove_file(&file).unwrap();
        symlink(&outside, path.join(PENDING_CHATGPT_ACCOUNTS_FILE)).unwrap();
        assert!(state.replace_chatgpt_accounts_record(b"new").is_err());
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
            SAVED_ACCOUNT_FILE,
            CHATGPT_REGISTRATION_FILE,
            CHATGPT_ACCOUNTS_FILE,
            PROVIDER_PROFILES_FILE,
            "model-preferences.json",
            "ui-preferences.json",
        ] {
            mkfifoat(CWD, path.join(name), Mode::RUSR | Mode::WUSR).expect("test FIFO");
        }
        assert!(matches!(
            state.read_saved_account_record(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.saved_account_record_present(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.read_chatgpt_registration_record(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.read_chatgpt_accounts_record(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.chatgpt_accounts_record_present(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.read_provider_profiles(),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.read_model_preferences_record(),
            Err(StoreError::StateNotPrivate)
        ));

        assert!(matches!(
            state.read_ui_preferences_record(),
            Err(StoreError::StateNotPrivate)
        ));
        std::fs::remove_file(path.join("ui-preferences.json")).unwrap();
        std::fs::remove_file(path.join(SAVED_ACCOUNT_FILE)).expect("remove account FIFO");
        std::fs::remove_file(path.join("model-preferences.json")).expect("remove preference FIFO");
        for name in [
            PENDING_ACCOUNT_FILE,
            PENDING_CHATGPT_REGISTRATION_FILE,
            PENDING_CHATGPT_ACCOUNTS_FILE,
            "account-credentials.lock",
            "model-preferences.pending",
            "ui-preferences.pending",
        ] {
            mkfifoat(CWD, path.join(name), Mode::RUSR | Mode::WUSR).expect("test FIFO");
        }
        assert!(matches!(
            state.replace_saved_account_record(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.replace_chatgpt_registration_record(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.replace_chatgpt_accounts_record(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.replace_model_preferences_record(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        assert!(matches!(
            state.with_account_replacement_lock(&workspace, || ()),
            Err(StoreError::StateNotPrivate)
        ));

        assert!(matches!(
            state.replace_ui_preferences_record(b"synthetic"),
            Err(StoreError::StateNotPrivate)
        ));
        drop(state);
        std::fs::remove_file(path.join(DATABASE_FILE)).expect("remove database");
        mkfifoat(CWD, path.join(DATABASE_FILE), Mode::RUSR | Mode::WUSR).expect("test FIFO");
        assert!(matches!(
            StateRoot::open_existing(&path),
            Err(StoreError::StateNotPrivate)
        ));
    }
}

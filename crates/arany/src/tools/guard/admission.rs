#[cfg(target_os = "macos")]
const ROOT_SCAN_LIMIT: usize = 4096;
pub(super) const RECORD_BYTES_CAP: usize = 2048;
use crate::private_fs::{PrivateDirectory, open_directory};
use crate::tools::ToolError;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{File, MetadataExt, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const MAX_WAITERS: usize = 1024;
const ADMISSION_SCAN_LIMIT: usize = MAX_WAITERS + 64 * 4 + 16;
const RESERVATION_VERSION: u32 = if cfg!(target_os = "linux") { 2 } else { 1 };
#[cfg(target_os = "linux")]
const SEED_ENTRY_LIMIT: usize =
    (crate::tools::types::MAX_SNAPSHOT_FILES + crate::tools::types::MAX_SKILLS * 32 + 12 * 17)
        * 260;
pub(super) const RECOVERY_ATTEMPTS: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    version: u32,
    units: usize,
    queued: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            version: 1,
            units: 8,
            queued: 32,
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<(), ToolError> {
        if self.version != 1
            || !(1..=64).contains(&self.units)
            || !(1..=MAX_WAITERS).contains(&self.queued)
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    version: u32,
    next: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Waiting {
    version: u32,
    expires_at_ms: u64,
}

struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

fn checked_file(dir: &cap_std::fs::Dir, name: &str, create: bool) -> Result<File, ToolError> {
    open_file(dir, name, create)?.ok_or(ToolError::Path)
}

pub(super) fn open_file(
    dir: &cap_std::fs::Dir,
    name: &str,
    create: bool,
) -> Result<Option<File>, ToolError> {
    let mut existing = OpenOptions::new();
    existing
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .follow(FollowSymlinks::No);
    let file = match dir.open_with(name, &existing) {
        Ok(file) => file,
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NONBLOCK)
                .follow(FollowSymlinks::No);
            match dir.open_with(name, &options) {
                Ok(file) => file,
                // Reopen on AlreadyExists: concurrent macOS no-follow create(true) can report NotFound.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => dir
                    .open_with(name, &existing)
                    .map_err(|_| ToolError::Path)?,
                Err(_) => return Err(ToolError::Path),
            }
        }
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ToolError::Path),
    };
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !create && metadata.nlink() == 0 {
        return Ok(None);
    }
    let acl = crate::private_fs::check_acl(&file, true);
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > RECORD_BYTES_CAP as u64
        || acl.is_err()
    {
        return Err(ToolError::Path);
    }
    Ok(Some(file))
}

fn decode<T: serde::de::DeserializeOwned>(file: File) -> Result<T, ToolError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(RECORD_BYTES_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::Path)?;
    if bytes.len() > RECORD_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    serde_json::from_slice(&bytes).map_err(|_| ToolError::ProtectionUnavailable)
}

fn remove(dir: &cap_std::fs::Dir, name: &str) -> Result<(), ToolError> {
    match dir.remove_file(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ToolError::Operation),
    }
}

fn reap_publications(dir: &cap_std::fs::Dir) -> Result<(), ToolError> {
    for (scanned, entry) in dir.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if scanned >= 4096 {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ToolError::Path)?;
        let Some(base) = name.strip_suffix(".new") else {
            continue;
        };
        let valid = base
            .strip_suffix(".unit")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            || base
                .strip_suffix(".wait")
                .is_some_and(|seq| seq.parse::<u64>().is_ok());
        if !valid {
            return Err(ToolError::Path);
        }
        let Some(file) = open_file(dir, name, false)? else {
            continue;
        };
        if let Some(_dead) = try_lock(file)? {
            remove(dir, name)?;
        }
    }
    Ok(())
}

fn try_lock(file: File) -> Result<Option<Lock>, ToolError> {
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(Lock(file))),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(_) => Err(ToolError::ProtectionUnavailable),
    }
}

fn record<T: serde::de::DeserializeOwned>(
    dir: &cap_std::fs::Dir,
    name: &str,
) -> Result<T, ToolError> {
    read_record(dir, name)
}

pub(super) fn replace_record(
    dir: &cap_std::fs::Dir,
    name: &str,
    value: &impl Serialize,
) -> Result<(), ToolError> {
    let temporary = format!("{name}.next");
    match dir.symlink_metadata(&temporary) {
        Ok(_) => {
            let _ = checked_file(dir, &temporary, false)?;
            dir.remove_file(&temporary)
                .map_err(|_| ToolError::Operation)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ToolError::Path),
    }
    write_record(dir, name, value)
}

fn limits(dir: &cap_std::fs::Dir) -> Result<Limits, ToolError> {
    let limits = match dir.symlink_metadata("limits.json") {
        Ok(_) => record(dir, "limits.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let limits = Limits::default();
            replace_record(dir, "limits.json", &limits)?;
            limits
        }
        Err(_) => return Err(ToolError::Configuration),
    };
    limits.validate()?;
    Ok(limits)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    version: u32,
    id: uuid::Uuid,
    boot: uuid::Uuid,
}

fn boot() -> Result<uuid::Uuid, ToolError> {
    #[cfg(target_os = "macos")]
    {
        super::macos::boot_session()
    }
    #[cfg(target_os = "linux")]
    {
        let bytes = crate::tools::fs::read_regular(
            crate::tools::fs::open_absolute("/proc/sys/kernel/random/boot_id")?,
            64,
        )?;
        uuid::Uuid::parse_str(
            std::str::from_utf8(&bytes)
                .map_err(|_| ToolError::Path)?
                .trim(),
        )
        .map_err(|_| ToolError::ProtectionUnavailable)
    }
}

fn reserved(dir: &cap_std::fs::Dir, _root_parent: &Path) -> Result<usize, ToolError> {
    let mut ids = std::collections::BTreeSet::new();
    for (scanned, entry) in dir.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if scanned >= ADMISSION_SCAN_LIMIT {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ToolError::Path)?;
        let Some(id) = name.strip_suffix(".unit") else {
            continue;
        };
        let id = uuid::Uuid::parse_str(id).map_err(|_| ToolError::Path)?;
        if name != format!("{id}.unit") {
            return Err(ToolError::Path);
        }
        let Some(file) = open_file(dir, name, false)? else {
            continue;
        };
        let reservation: Reservation = decode(file.try_clone().map_err(|_| ToolError::Operation)?)?;
        if reservation.version != RESERVATION_VERSION
            || reservation.id != id
            || reservation.boot.is_nil()
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        #[cfg(target_os = "macos")]
        if let Some(_dead) = try_lock(file)? {
            let removable = !_root_parent
                .join(format!("arany-u-{id}"))
                .try_exists()
                .map_err(|_| ToolError::Path)?;
            if removable {
                remove(dir, name)?;
                continue;
            }
        }
        ids.insert(id.to_string());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        for (scanned, entry) in std::fs::read_dir(_root_parent)
            .map_err(|_| ToolError::Path)?
            .enumerate()
        {
            if scanned >= ROOT_SCAN_LIMIT {
                return Err(ToolError::Limit);
            }
            let entry = entry.map_err(|_| ToolError::Path)?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_prefix("arany-u-")) else {
                continue;
            };
            match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) if metadata.uid() == rustix::process::geteuid().as_raw() => {
                    ids.insert(id.to_owned());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(ToolError::Path),
            }
        }
    }
    Ok(ids.len())
}

pub(super) struct Permit {
    dir: cap_std::fs::Dir,
    name: String,
    _lease: Lock,
    pub(super) id: uuid::Uuid,
    #[cfg(target_os = "linux")]
    seed_parent: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    boot: uuid::Uuid,
    release_on_drop: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.release_on_drop {
            let _ = self.dir.remove_file(&self.name);
        }
        #[cfg(target_os = "linux")]
        if self.release_on_drop && self.cleanup_seed().is_ok() {
            let _ = self.retire();
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedRecord {
    version: u32,
    id: uuid::Uuid,
    boot: uuid::Uuid,
    path: std::path::PathBuf,
    parent: (u64, u64),
    phase: SeedPhase,
}

#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum SeedPhase {
    Preparing,
    Ready { root: (u64, u64) },
    LaunchRequested { root: (u64, u64) },
    Terminated { root: (u64, u64) },
}

#[cfg(target_os = "linux")]
fn namespace_lock(dir: &cap_std::fs::Dir) -> Result<Option<Lock>, ToolError> {
    let file = checked_file(dir, "admission.lock", true)?;
    if file.metadata().map_err(|_| ToolError::Path)?.len() != 0 {
        return Err(ToolError::Path);
    }
    try_lock(file)
}

#[cfg(target_os = "linux")]
fn sync_directory(dir: &cap_std::fs::Dir) -> Result<(), ToolError> {
    crate::tools::fs::open_file(dir, ".")?
        .sync_all()
        .map_err(|_| ToolError::Operation)
}

#[cfg(target_os = "linux")]
impl Permit {
    fn linked(&self) -> Result<bool, ToolError> {
        let owned = self._lease.0.metadata().map_err(|_| ToolError::Path)?;
        let Some(named) = open_file(&self.dir, &self.name, false)? else {
            return Ok(false);
        };
        let named = named.metadata().map_err(|_| ToolError::Path)?;
        Ok(owned.nlink() == 1 && (owned.dev(), owned.ino()) == (named.dev(), named.ino()))
    }

    fn seed_name(&self) -> String {
        format!("{}.seed", self.id)
    }

    fn seed_path(&self) -> std::path::PathBuf {
        self.seed_parent.join(format!("arany-tools-{}", self.id))
    }

    fn seed_record(&self) -> Result<Option<SeedRecord>, ToolError> {
        let Some(file) = open_file(&self.dir, &self.seed_name(), false)? else {
            return Ok(None);
        };
        let seed: SeedRecord = decode(file)?;
        if seed.version != 1
            || seed.id != self.id
            || seed.boot != self.boot
            || seed.path != self.seed_path()
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(Some(seed))
    }

    pub(super) fn prepare_seed(
        &self,
        workspace: &cap_std::fs::Dir,
        config: &crate::tools::config::Config,
        include_workspace: bool,
    ) -> Result<(std::path::PathBuf, cap_std::fs::Dir), ToolError> {
        use cap_fs_ext::DirExt;
        use cap_std::fs::DirBuilderExt;
        if self.seed_record()?.is_some() {
            return Err(ToolError::ProtectionUnavailable);
        }
        let parent = crate::tools::fs::open_directory(&self.seed_parent)?;
        match parent.symlink_metadata(format!("arany-tools-{}", self.id)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(ToolError::Path),
        }
        let metadata = parent.dir_metadata().map_err(|_| ToolError::Path)?;
        if !((metadata.uid() == 0 && metadata.mode() & 0o1000 != 0)
            || (metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o077 == 0))
        {
            return Err(ToolError::Path);
        }
        let mut seed = SeedRecord {
            version: 1,
            id: self.id,
            boot: self.boot,
            path: self.seed_path(),
            parent: crate::tools::fs::identity(&parent)?,
            phase: SeedPhase::Preparing,
        };
        replace_record(&self.dir, &self.seed_name(), &seed)?;
        #[cfg(test)]
        self.seed_checkpoint("planned");
        let name = seed.path.file_name().ok_or(ToolError::Path)?;
        parent
            .create_dir_with(name, cap_std::fs::DirBuilder::new().mode(0o700))
            .map_err(|_| ToolError::Operation)?;
        sync_directory(&parent)?;
        #[cfg(test)]
        self.seed_checkpoint("created");
        let checked = open_directory(&seed.path, false).map_err(|_| ToolError::Path)?;
        let dir = cap_std::fs::Dir::from_std_file(crate::tools::fs::open_file(
            &parent,
            name.to_str().ok_or(ToolError::Path)?,
        )?);
        let root = crate::tools::fs::identity(&dir)?;
        if root != crate::tools::fs::identity(&checked.dir)? {
            return Err(ToolError::Path);
        }
        seed.phase = SeedPhase::Ready { root };
        replace_record(&self.dir, &self.seed_name(), &seed)?;
        #[cfg(test)]
        self.seed_checkpoint("owned");
        dir.create_dir_with("workspace", cap_std::fs::DirBuilder::new().mode(0o700))
            .map_err(|_| ToolError::Operation)?;
        let project = dir
            .open_dir_nofollow("workspace")
            .map_err(|_| ToolError::Path)?;
        crate::tools::fs::snapshot_into(
            workspace,
            config,
            include_workspace.then_some(&project),
            &dir,
        )?;
        #[cfg(test)]
        self.seed_checkpoint("copied");
        Ok((seed.path, dir))
    }

    pub(super) fn launching(&self) -> Result<(), ToolError> {
        let mut seed = self
            .seed_record()?
            .ok_or(ToolError::ProtectionUnavailable)?;
        let SeedPhase::Ready { root } = seed.phase else {
            return Err(ToolError::ProtectionUnavailable);
        };
        seed.phase = SeedPhase::LaunchRequested { root };
        replace_record(&self.dir, &self.seed_name(), &seed)
    }

    pub(super) fn terminated(&self) -> Result<(), ToolError> {
        let mut seed = self
            .seed_record()?
            .ok_or(ToolError::ProtectionUnavailable)?;
        let SeedPhase::LaunchRequested { root } = seed.phase else {
            return Err(ToolError::ProtectionUnavailable);
        };
        seed.phase = SeedPhase::Terminated { root };
        replace_record(&self.dir, &self.seed_name(), &seed)
    }

    fn cleanup_seed(&self) -> Result<(), ToolError> {
        if !self.linked()? {
            return Err(ToolError::ProtectionUnavailable);
        }
        let seed = self.seed_record()?;
        let Some(seed) = seed else { return Ok(()) };
        let root = match seed.phase {
            SeedPhase::Preparing => None,
            SeedPhase::Ready { root } | SeedPhase::Terminated { root } => Some(root),
            SeedPhase::LaunchRequested { .. } => return Err(ToolError::ProtectionUnavailable),
        };
        let parent = crate::tools::fs::open_directory(&self.seed_parent)?;
        if crate::tools::fs::identity(&parent)? != seed.parent {
            return Err(ToolError::Path);
        }
        let name = seed
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ToolError::Path)?;
        match parent.symlink_metadata(name) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return sync_directory(&parent);
            }
            Err(_) => return Err(ToolError::Path),
        }
        let dir = cap_std::fs::Dir::from_std_file(crate::tools::fs::open_file(&parent, name)?);
        let metadata = dir.dir_metadata().map_err(|_| ToolError::Path)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(ToolError::Path);
        }
        let actual = crate::tools::fs::identity(&dir)?;
        if let Some(root) = root {
            if actual != root {
                return Err(ToolError::Path);
            }
            let mut remaining = SEED_ENTRY_LIMIT;
            remove_seed_contents(&dir, &mut remaining, 0)?;
        }
        let named = parent.symlink_metadata(name).map_err(|_| ToolError::Path)?;
        if !named.is_dir() || (named.dev(), named.ino()) != actual {
            return Err(ToolError::Path);
        }
        parent.remove_dir(name).map_err(|_| ToolError::Operation)?;
        sync_directory(&parent)?;
        #[cfg(test)]
        self.seed_checkpoint("removed");
        Ok(())
    }

    fn retire(&self) -> Result<bool, ToolError> {
        let Some(_namespace) = namespace_lock(&self.dir)? else {
            return Ok(false);
        };
        if !self.linked()? {
            return Ok(true);
        }
        for name in [format!("{}.next", self.seed_name()), self.seed_name()] {
            if open_file(&self.dir, &name, false)?.is_some() {
                remove(&self.dir, &name)?;
            }
        }
        sync_directory(&self.dir)?;
        remove(&self.dir, &self.name)?;
        sync_directory(&self.dir)?;
        Ok(true)
    }

    pub(super) async fn cleanup(&self) -> Result<(), ToolError> {
        self.cleanup_seed()?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.retire()? {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Ok(())
        })
        .await
        .map_err(|_| ToolError::ProtectionUnavailable)?
    }

    #[cfg(test)]
    pub(super) fn seed_checkpoint(&self, stage: &str) {
        use std::io::Read;
        if std::env::var("ARANY_TEST_SEED_STAGE").as_deref() != Ok(stage) {
            return;
        }
        let path = std::env::var_os("ARANY_TEST_ADMISSION").expect("private seed fixture");
        let mut stream =
            std::os::unix::net::UnixStream::connect(Path::new(&path).join("control")).unwrap();
        writeln!(stream, "{}", self.id).unwrap();
        stream.read_exact(&mut [0]).unwrap();
    }
}

#[cfg(target_os = "linux")]
fn remove_seed_contents(
    dir: &cap_std::fs::Dir,
    remaining: &mut usize,
    depth: usize,
) -> Result<(), ToolError> {
    if depth > 260 {
        return Err(ToolError::Limit);
    }
    for entry in dir.entries().map_err(|_| ToolError::Path)? {
        *remaining = remaining.checked_sub(1).ok_or(ToolError::Limit)?;
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ToolError::Path)?;
        let file = crate::tools::fs::open_file(dir, name)?;
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(ToolError::Path);
        }
        if metadata.is_dir() {
            let child = cap_std::fs::Dir::from_std_file(file);
            remove_seed_contents(&child, remaining, depth + 1)?;
            let named = dir.symlink_metadata(name).map_err(|_| ToolError::Path)?;
            if !named.is_dir() || (named.dev(), named.ino()) != (metadata.dev(), metadata.ino()) {
                return Err(ToolError::Path);
            }
            dir.remove_dir(name).map_err(|_| ToolError::Operation)?;
        } else if metadata.is_file() && metadata.nlink() == 1 {
            let named = dir.symlink_metadata(name).map_err(|_| ToolError::Path)?;
            if !named.is_file() || (named.dev(), named.ino()) != (metadata.dev(), metadata.ino()) {
                return Err(ToolError::Path);
            }
            dir.remove_file(name).map_err(|_| ToolError::Operation)?;
        } else {
            return Err(ToolError::Path);
        }
    }
    sync_directory(dir)
}

fn reserve(
    directory: &PrivateDirectory,
    id: uuid::Uuid,
    _root_parent: &Path,
) -> Result<Permit, ToolError> {
    use std::io::Write;
    let name = format!("{id}.unit");
    let mut file = directory
        .dir
        .open_with(
            format!("{name}.new"),
            OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No)
                .custom_flags(nix::libc::O_NONBLOCK),
        )
        .map_err(|_| ToolError::Operation)?;
    let permit = Permit {
        dir: directory
            .dir
            .try_clone()
            .map_err(|_| ToolError::Operation)?,
        name,
        _lease: try_lock(file.try_clone().map_err(|_| ToolError::Operation)?)?
            .ok_or(ToolError::ProtectionUnavailable)?,
        id,
        #[cfg(target_os = "linux")]
        seed_parent: _root_parent.to_owned(),
        #[cfg(target_os = "linux")]
        boot: boot()?,
        release_on_drop: true,
    };
    let bytes = serde_json::to_vec(&Reservation {
        version: RESERVATION_VERSION,
        id,
        boot: boot()?,
    })
    .map_err(|_| ToolError::Operation)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| ToolError::Operation)?;
    directory
        .dir
        .rename(format!("{}.new", permit.name), &directory.dir, &permit.name)
        .map_err(|_| ToolError::Operation)?;
    directory
        .dir
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|_| ToolError::Operation)?;
    Ok(permit)
}

fn waiting(dir: &cap_std::fs::Dir, now_ms: u64) -> Result<Vec<u64>, ToolError> {
    let mut waiters = Vec::new();
    for (scanned, entry) in dir.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if scanned >= ADMISSION_SCAN_LIMIT {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ToolError::Path)?;
        let Some(sequence) = name.strip_suffix(".wait") else {
            continue;
        };
        let sequence: u64 = sequence.parse().map_err(|_| ToolError::Path)?;
        if name != format!("{sequence}.wait") {
            return Err(ToolError::Path);
        }
        let Some(file) = open_file(dir, name, false)? else {
            continue;
        };
        let waiter: Waiting = decode(file.try_clone().map_err(|_| ToolError::Operation)?)?;
        if let Some(_dead) = try_lock(file)? {
            remove(dir, name)?;
            continue;
        }
        if waiter.version != 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
        if waiter.expires_at_ms <= now_ms {
            remove(dir, name)?;
            continue;
        }
        waiters.push(sequence);
        if waiters.len() > MAX_WAITERS {
            return Err(ToolError::Limit);
        }
    }
    waiters.sort_unstable();
    Ok(waiters)
}

struct Ticket {
    directory: cap_std::fs::Dir,
    name: String,
    _lease: Lock,
    sequence: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = self.directory.remove_file(&self.name);
    }
}

fn register(directory: &PrivateDirectory, expires_at_ms: u64) -> Result<Ticket, ToolError> {
    use std::io::Write;
    let mut counter: Counter = match directory.dir.symlink_metadata("counter.json") {
        Ok(_) => record(&directory.dir, "counter.json")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Counter {
            version: 1,
            next: 1,
        },
        Err(_) => return Err(ToolError::Path),
    };
    if counter.version != 1 || counter.next == 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    let sequence = counter.next;
    counter.next = sequence.checked_add(1).ok_or(ToolError::Limit)?;
    replace_record(&directory.dir, "counter.json", &counter)?;
    let name = format!("{sequence}.wait");
    let mut file = directory
        .dir
        .open_with(
            format!("{name}.new"),
            OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No)
                .custom_flags(nix::libc::O_NONBLOCK),
        )
        .map_err(|_| ToolError::Operation)?;
    let ticket = Ticket {
        directory: directory
            .dir
            .try_clone()
            .map_err(|_| ToolError::Operation)?,
        name,
        _lease: try_lock(file.try_clone().map_err(|_| ToolError::Operation)?)?
            .ok_or(ToolError::ProtectionUnavailable)?,
        sequence,
    };
    let bytes = serde_json::to_vec(&Waiting {
        version: 1,
        expires_at_ms,
    })
    .map_err(|_| ToolError::Operation)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| ToolError::Operation)?;
    directory
        .dir
        .rename(format!("{}.new", ticket.name), &directory.dir, &ticket.name)
        .map_err(|_| ToolError::Operation)?;
    directory
        .dir
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|_| ToolError::Operation)?;
    Ok(ticket)
}

pub(super) async fn acquire(
    id: uuid::Uuid,
    expires_at_ms: u64,
    queued: impl Fn(bool),
) -> Result<Permit, ToolError> {
    let path = crate::store::StateRoot::account_path()
        .map_err(|_| ToolError::Path)?
        .join("guard");
    let directory = open_directory(&path, true).map_err(|_| ToolError::Path)?;
    acquire_in(
        &directory,
        Path::new(if cfg!(target_os = "linux") {
            "/tmp"
        } else {
            "/private/tmp"
        }),
        id,
        expires_at_ms,
        queued,
        crate::tools::now_ms,
    )
    .await
}

pub(super) async fn recover_private(ids: &[uuid::Uuid]) -> Result<(), ToolError> {
    if ids.len() > 8 * 17 || ids.iter().any(|id| id.is_nil()) {
        return Err(ToolError::Limit);
    }
    let path = crate::store::StateRoot::account_path()
        .map_err(|_| ToolError::Path)?
        .join("guard");
    let directory = open_directory(&path, true).map_err(|_| ToolError::Path)?;
    let parent = Path::new(if cfg!(target_os = "linux") {
        "/tmp"
    } else {
        "/private/tmp"
    });
    recover_private_in(&directory, parent, ids).await
}

async fn recover_private_in(
    directory: &PrivateDirectory,
    parent: &Path,
    ids: &[uuid::Uuid],
) -> Result<(), ToolError> {
    let mut recovery = RecoveryBudget {
        remaining: Duration::from_secs(30),
        attempts: RECOVERY_ATTEMPTS,
    };
    let now = crate::tools::now_ms();
    recovery
        .recover(directory, parent, now.saturating_add(30_000), now)
        .await?;
    for id in ids {
        for suffix in ["unit", "seed", "seed.next"] {
            if open_file(&directory.dir, &format!("{id}.{suffix}"), false)?.is_some() {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        #[cfg(target_os = "linux")]
        match std::fs::symlink_metadata(parent.join(format!("arany-tools-{id}"))) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(ToolError::ProtectionUnavailable),
        }
        #[cfg(target_os = "macos")]
        super::macos::private_recovered(parent, *id)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn recover_reservations(
    dir: &cap_std::fs::Dir,
    root_parent: &Path,
    attempts: &mut usize,
) -> Result<(), ToolError> {
    for (scanned, entry) in dir.entries().map_err(|_| ToolError::Path)?.enumerate() {
        if scanned >= 4096 {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ToolError::Path)?;
        let Some(id) = name.strip_suffix(".unit") else {
            continue;
        };
        let id = uuid::Uuid::parse_str(id).map_err(|_| ToolError::Path)?;
        let Some(file) = open_file(dir, name, false)? else {
            continue;
        };
        let reservation: Reservation = decode(file.try_clone().map_err(|_| ToolError::Operation)?)?;
        if reservation.version != RESERVATION_VERSION
            || reservation.id != id
            || reservation.boot.is_nil()
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let Some(lease) = try_lock(file)? else {
            continue;
        };
        let permit = Permit {
            dir: dir.try_clone().map_err(|_| ToolError::Operation)?,
            name: name.to_owned(),
            _lease: lease,
            id,
            seed_parent: root_parent.to_owned(),
            boot: reservation.boot,
            release_on_drop: false,
        };
        let Some(namespace) = namespace_lock(dir)? else {
            continue;
        };
        if !permit.linked()? {
            continue;
        }
        let seed = permit.seed_record()?;
        drop(namespace);
        if *attempts == 0 {
            break;
        }
        *attempts -= 1;
        if seed
            .as_ref()
            .is_some_and(|seed| matches!(seed.phase, SeedPhase::LaunchRequested { .. }))
        {
            if reservation.boot == boot()?
                && !super::linux::recover_reservation(id).await.unwrap_or(false)
            {
                continue;
            }
            permit.terminated()?;
        }
        permit.cleanup().await?;
    }
    Ok(())
}

struct RecoveryBudget {
    remaining: Duration,
    attempts: usize,
}

impl RecoveryBudget {
    async fn recover(
        &mut self,
        directory: &PrivateDirectory,
        root_parent: &Path,
        expiry: u64,
        now_ms: u64,
    ) -> Result<(), ToolError> {
        if self.attempts == 0 || self.remaining.is_zero() {
            return Ok(());
        }
        let deadline = self
            .remaining
            .min(Duration::from_millis(expiry.saturating_sub(now_ms)));
        let started = tokio::time::Instant::now();
        #[cfg(target_os = "macos")]
        let capture = super::macos::fixture::Span::begin(super::macos::fixture::Stage::Recovery);
        #[cfg(target_os = "macos")]
        let recovery = super::macos::recover_abandoned(root_parent, &mut self.attempts);
        #[cfg(target_os = "linux")]
        let recovery = recover_reservations(&directory.dir, root_parent, &mut self.attempts);
        #[cfg(target_os = "macos")]
        let _ = directory;
        let result = tokio::time::timeout(deadline, recovery)
            .await
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        self.remaining = self.remaining.saturating_sub(started.elapsed());
        #[cfg(target_os = "macos")]
        if result.is_ok() {
            capture.complete();
        }
        result
    }
}

pub(super) async fn acquire_in(
    directory: &PrivateDirectory,
    root_parent: &Path,
    id: uuid::Uuid,
    expires_at_ms: u64,
    queued: impl Fn(bool),
    now: impl Fn() -> u64,
) -> Result<Permit, ToolError> {
    let mut ticket: Option<Ticket> = None;
    let mut recovery = RecoveryBudget {
        remaining: Duration::from_secs(30),
        attempts: RECOVERY_ATTEMPTS,
    };
    recovery
        .recover(directory, root_parent, expires_at_ms, now())
        .await?;
    loop {
        if now() >= expires_at_ms {
            return Err(ToolError::Limit);
        }
        let file = checked_file(&directory.dir, "admission.lock", true)?;
        if file.metadata().map_err(|_| ToolError::Path)?.len() != 0 {
            return Err(ToolError::Path);
        }
        let mut recover_capacity = false;
        if let Some(_lock) = try_lock(file)? {
            reap_publications(&directory.dir)?;
            let limits = limits(&directory.dir)?;
            let waiters = waiting(&directory.dir, now())?;
            let first = match &ticket {
                Some(ticket) => waiters.first() == Some(&ticket.sequence),
                None => waiters.is_empty(),
            };
            if first && reserved(&directory.dir, root_parent)? < limits.units {
                if now() >= expires_at_ms {
                    return Err(ToolError::Limit);
                }
                let unit = reserve(directory, id, root_parent)?;
                drop(ticket);
                queued(false);
                return Ok(unit);
            }
            recover_capacity = first;
            if ticket.is_none() {
                if waiters.len() >= limits.queued {
                    return Err(ToolError::QueueFull);
                }
                ticket = Some(register(directory, expires_at_ms)?);
                queued(true);
            }
        }
        if recover_capacity {
            recovery
                .recover(directory, root_parent, expires_at_ms, now())
                .await?;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(super) fn read_record<T: serde::de::DeserializeOwned>(
    directory: &cap_std::fs::Dir,
    name: &str,
) -> Result<T, ToolError> {
    use cap_fs_ext::OpenOptionsFollowExt;
    use cap_std::fs::{MetadataExt, OpenOptionsExt};
    use std::io::Read;
    let file = directory
        .open_with(
            name,
            cap_std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .follow(cap_fs_ext::FollowSymlinks::No),
        )
        .map_err(|_| ToolError::Path)?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > RECORD_BYTES_CAP as u64
        || crate::private_fs::check_acl(&file, true).is_err()
    {
        return Err(ToolError::Path);
    }
    let mut bytes = Vec::new();
    file.take(RECORD_BYTES_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::Path)?;
    if bytes.len() > RECORD_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    serde_json::from_slice(&bytes).map_err(|_| ToolError::ProtectionUnavailable)
}

pub(super) fn write_record(
    directory: &cap_std::fs::Dir,
    name: &str,
    record: &impl Serialize,
) -> Result<(), ToolError> {
    use cap_fs_ext::OpenOptionsFollowExt;
    use cap_std::fs::OpenOptionsExt;
    let bytes = serde_json::to_vec(record).map_err(|_| ToolError::Operation)?;
    if bytes.len() > RECORD_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    let temporary = format!("{name}.next");
    let mut file = directory
        .open_with(
            &temporary,
            cap_std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .follow(cap_fs_ext::FollowSymlinks::No),
        )
        .map_err(|_| ToolError::Operation)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| ToolError::Operation)?;
    directory
        .rename(&temporary, directory, name)
        .map_err(|_| ToolError::Operation)?;
    directory
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|_| ToolError::Operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use tokio::io::BufReader;

    const HELPER: &str = "tools::guard::admission::tests::admission_process";

    #[cfg(target_os = "linux")]
    fn seed_config() -> crate::tools::config::Config {
        crate::tools::config::Config {
            version: 1,
            workspace_paths: vec!["sample".into()],
            write: false,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
            commands: Vec::new(),
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants: Vec::new(),
            git: None,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper-only: activated by Linux seed ownership corpus"]
    fn seed_process() {
        let path = PathBuf::from(std::env::var_os("ARANY_TEST_ADMISSION").unwrap());
        let stage = std::env::var("ARANY_TEST_SEED_STAGE").unwrap();
        let id = uuid::Uuid::parse_str(&std::env::var("ARANY_TEST_SEED_ID").unwrap()).unwrap();
        rustix::process::umask(rustix::fs::Mode::empty());
        let reactor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        reactor.block_on(async {
            let directory = open_directory(&path.join("guard"), true).unwrap();
            if std::env::var_os("ARANY_TEST_SEED_RECOVER").is_some() {
                recover_reservations(&directory.dir, &path, &mut 8)
                    .await
                    .unwrap();
                return;
            }
            let workspace = open_directory(&path.join("source"), true).unwrap();
            workspace
                .dir
                .write("sample", b"private seed canary")
                .unwrap();
            workspace.dir.create_dir_all("nested/empty").unwrap();
            let permit = acquire_in(
                &directory,
                &path,
                id,
                crate::tools::now_ms() + 60_000,
                |_| {},
                crate::tools::now_ms,
            )
            .await
            .unwrap();
            permit.seed_checkpoint("reserved");
            let mut config = seed_config();
            config.workspace_paths.push("nested".into());
            if stage == "partial" {
                config.commands.push(crate::tools::config::Program {
                    name: "missing".into(),
                    executable: "/arany-nonexistent-runtime".into(),
                    sha256: "0".repeat(64),
                    interpreter: false,
                    inputs: Vec::new(),
                });
            }
            let prepared = permit.prepare_seed(&workspace.dir, &config, true);
            if stage == "partial" {
                assert!(prepared.is_err());
                permit.seed_checkpoint("partial");
            } else {
                prepared.unwrap();
            }
            if stage == "native" {
                super::super::linux::hold_seed_unit(&permit, &permit.seed_path()).await;
                return;
            }
            permit.launching().unwrap();
            permit.seed_checkpoint("launching");
            permit.terminated().unwrap();
            permit.seed_checkpoint("terminated");
            permit.cleanup().await.unwrap();
        });
    }

    #[cfg(target_os = "linux")]
    struct SeedController {
        child: Child,
        native: Option<uuid::Uuid>,
    }

    #[cfg(target_os = "linux")]
    impl Drop for SeedController {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(id) = self.native {
                super::super::linux::stop_seed_fixture(id);
            }
        }
    }

    #[cfg(target_os = "linux")]
    async fn start_seed_controller(
        path: &Path,
        listener: &tokio::net::UnixListener,
        stage: &str,
        id: uuid::Uuid,
        recover: bool,
    ) -> (SeedController, BufReader<tokio::net::UnixStream>) {
        let mut child = SeedController {
            child: Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "tools::guard::admission::tests::seed_process",
                    "--nocapture",
                ])
                .env_clear()
                .env("ARANY_TEST_ADMISSION", path)
                .env("ARANY_TEST_SEED_STAGE", stage)
                .env("ARANY_TEST_SEED_ID", id.to_string())
                .envs(recover.then_some(("ARANY_TEST_SEED_RECOVER", "1")))
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            native: (stage == "native").then_some(id),
        };
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        let (stream, _) = match accepted {
            Ok(Ok(accepted)) => accepted,
            _ => {
                let _ = child.child.kill();
                let observed =
                    crate::test_process::capture(&mut child.child, Duration::from_secs(5), 8192);
                panic!(
                    "seed stage {stage} never reached: {:?}",
                    String::from_utf8_lossy(&observed.stderr)
                );
            }
        };
        let mut reader = BufReader::new(stream);
        let line = tokio::time::timeout(
            Duration::from_secs(5),
            super::super::read_line_capped(&mut reader, 64),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            uuid::Uuid::parse_str(std::str::from_utf8(&line).unwrap().trim()).unwrap(),
            id
        );
        (child, reader)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn seed_ownership_survives_controller_death_at_each_publication_boundary() {
        seed_death_cases(&[
            "reserved",
            "planned",
            "created",
            "owned",
            "partial",
            "copied",
            "launching",
            "terminated",
            "removed",
        ])
        .await;
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(not(arany_ci), ignore = "requires native Linux systemd and Bubblewrap")]
    fn native_seed_recovery_terminates_the_owned_unit_before_releasing_storage() {
        let _gate = crate::test_process::NATIVE_GATE.lock().unwrap();
        super::super::linux::check_available().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(seed_death_cases(&["native"]));
    }

    #[cfg(target_os = "linux")]
    async fn seed_death_cases(stages: &[&str]) {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::ExitStatusExt;
        for &stage in stages {
            let fixture = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let listener = UnixListener::bind(fixture.path().join("control")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let id = uuid::Uuid::now_v7();
            let (mut child, _control) =
                start_seed_controller(fixture.path(), &listener, stage, id, false).await;
            let directory = open_directory(&fixture.path().join("guard"), false).unwrap();
            let seed_path = fixture.path().join(format!("arany-tools-{id}"));
            let native_group = if stage == "native" {
                Some(super::super::linux::seed_unit_group(id).await)
            } else {
                None
            };
            let present = !matches!(stage, "reserved" | "planned" | "removed");
            assert_eq!(seed_path.exists(), present, "stage {stage}");
            if matches!(
                stage,
                "partial" | "copied" | "launching" | "terminated" | "native"
            ) {
                assert_eq!(
                    std::fs::read(seed_path.join("workspace/sample")).unwrap(),
                    b"private seed canary"
                );
            }
            recover_reservations(&directory.dir, fixture.path(), &mut 8)
                .await
                .unwrap();
            assert_eq!(
                reserved(&directory.dir, fixture.path()).unwrap(),
                1,
                "live stage {stage}"
            );
            assert!(
                recover_private_in(&directory, fixture.path(), &[id])
                    .await
                    .is_err(),
                "live private recovery {stage}"
            );
            child.child.kill().unwrap();
            let observed =
                crate::test_process::capture(&mut child.child, Duration::from_secs(5), 8192);
            assert_eq!(observed.status.signal(), Some(9), "stage {stage}");
            recover_reservations(&directory.dir, fixture.path(), &mut 0)
                .await
                .unwrap();
            assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 1);
            if stage == "copied" {
                let (mut reaper, _control) =
                    start_seed_controller(fixture.path(), &listener, "removed", id, true).await;
                let mut attempts = 8;
                recover_reservations(&directory.dir, fixture.path(), &mut attempts)
                    .await
                    .unwrap();
                assert_eq!(
                    attempts, 8,
                    "a competing reaper cannot acquire the live lease"
                );
                assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 1);
                assert!(!seed_path.exists());
                reaper.child.kill().unwrap();
                let observed =
                    crate::test_process::capture(&mut reaper.child, Duration::from_secs(5), 8192);
                assert_eq!(observed.status.signal(), Some(9));
            }
            if stage == "launching" {
                recover_reservations(&directory.dir, fixture.path(), &mut 8)
                    .await
                    .unwrap();
                assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 1);
                assert!(seed_path.exists());
                let previous_boot = uuid::Uuid::now_v7();
                let mut reservation: Reservation =
                    read_record(&directory.dir, &format!("{id}.unit")).unwrap();
                reservation.boot = previous_boot;
                replace_record(&directory.dir, &format!("{id}.unit"), &reservation).unwrap();
                let mut seed: SeedRecord =
                    read_record(&directory.dir, &format!("{id}.seed")).unwrap();
                seed.boot = previous_boot;
                replace_record(&directory.dir, &format!("{id}.seed"), &seed).unwrap();
            }
            recover_reservations(&directory.dir, fixture.path(), &mut 8)
                .await
                .unwrap();
            assert!(!seed_path.exists(), "recovered stage {stage}");
            recover_private_in(&directory, fixture.path(), &[id])
                .await
                .unwrap();
            if let Some(group) = native_group {
                match group.read_to_string("cgroup.events") {
                    Ok(events) => assert!(events.lines().any(|line| line == "populated 0")),
                    Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
                }
            }
            assert_eq!(
                reserved(&directory.dir, fixture.path()).unwrap(),
                0,
                "recovered stage {stage}"
            );
            for suffix in ["unit", "seed", "seed.next"] {
                assert!(
                    !fixture
                        .path()
                        .join("guard")
                        .join(format!("{id}.{suffix}"))
                        .exists()
                );
            }
            let next = acquire_in(
                &directory,
                fixture.path(),
                uuid::Uuid::now_v7(),
                crate::tools::now_ms() + 60_000,
                |_| {},
                crate::tools::now_ms,
            )
            .await
            .unwrap();
            next.cleanup().await.unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn seed_cleanup_retains_capacity_on_unsafe_files_and_replaced_roots() {
        for include_workspace in [true, false] {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let fixture = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let directory = open_directory(&fixture.path().join("guard"), true).unwrap();
            let source = open_directory(&fixture.path().join("source"), true).unwrap();
            source.dir.write("sample", b"source").unwrap();
            let permit = acquire_in(
                &directory,
                fixture.path(),
                uuid::Uuid::now_v7(),
                crate::tools::now_ms() + 60_000,
                |_| {},
                crate::tools::now_ms,
            )
            .await
            .unwrap();
            let (path, _) = permit
                .prepare_seed(&source.dir, &seed_config(), include_workspace)
                .unwrap();
            let outside = fixture.path().join("outside");
            std::fs::write(&outside, b"unrelated").unwrap();
            symlink(&outside, path.join("unexpected")).unwrap();
            assert!(permit.cleanup().await.is_err());
            assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 1);
            assert_eq!(std::fs::read(&outside).unwrap(), b"unrelated");
            std::fs::remove_file(path.join("unexpected")).unwrap();
            let moved = fixture.path().join("retained");
            std::fs::rename(&path, &moved).unwrap();
            let replacement = open_directory(&path, true).unwrap();
            replacement.dir.write("outside", b"replacement").unwrap();
            assert!(permit.cleanup().await.is_err());
            assert_eq!(replacement.dir.read("outside").unwrap(), b"replacement");
            assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 1);
            std::fs::remove_file(path.join("outside")).unwrap();
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&moved, &path).unwrap();
            let stale = checked_file(&directory.dir, &permit.name, false).unwrap();
            permit.cleanup().await.unwrap();
            assert!(!path.exists());
            assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 0);
            assert!(!permit.linked().unwrap());
            assert!(permit.cleanup().await.is_err());
            let id = permit.id;
            drop(permit);
            let stale = Permit {
                dir: directory.dir.try_clone().unwrap(),
                name: format!("{id}.unit"),
                _lease: try_lock(stale).unwrap().unwrap(),
                id,
                seed_parent: fixture.path().to_owned(),
                boot: boot().unwrap(),
                release_on_drop: false,
            };
            let replacement = open_directory(&path, true).unwrap();
            replacement
                .dir
                .write("unrelated", b"keep after retirement")
                .unwrap();
            let _namespace = namespace_lock(&directory.dir).unwrap().unwrap();
            assert!(
                !stale.linked().unwrap(),
                "an opened-before-retirement lease is stale"
            );
            assert!(stale.cleanup_seed().is_err());
            assert_eq!(
                replacement.dir.read("unrelated").unwrap(),
                b"keep after retirement"
            );
        }
    }

    #[test]
    fn seed_metadata_preserves_maximum_admission_capacity() {
        let fixture = tempfile::tempdir().unwrap();
        let directory = open_directory(&fixture.path().join("guard"), true).unwrap();
        replace_record(
            &directory.dir,
            "limits.json",
            &Limits {
                version: 1,
                units: 64,
                queued: 1024,
            },
        )
        .unwrap();
        let mut permits = Vec::new();
        for _ in 0..limits(&directory.dir).unwrap().units {
            let mut permit = reserve(&directory, uuid::Uuid::now_v7(), fixture.path()).unwrap();
            permit.release_on_drop = false;
            for suffix in ["seed", "seed.next"] {
                checked_file(&directory.dir, &format!("{}.{suffix}", permit.id), true).unwrap();
            }
            permits.push(permit);
        }
        let tickets: Vec<_> = (0..1024)
            .map(|_| register(&directory, u64::MAX).unwrap())
            .collect();
        assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 64);
        assert_eq!(waiting(&directory.dir, 0).unwrap().len(), tickets.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn historical_seed_maintenance_removes_only_selected_inactive_objects() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let parent = open_directory(fixture.path(), false).unwrap();
        let directory = open_directory(&fixture.path().join("guard"), true).unwrap();
        let settings = limits(&directory.dir).unwrap();
        let id = uuid::Uuid::now_v7();
        let lease_name = format!("{id}.unit");
        replace_record(
            &directory.dir,
            &lease_name,
            &Reservation {
                version: 1,
                id,
                boot: boot().unwrap(),
            },
        )
        .unwrap();
        let lease = checked_file(&directory.dir, &lease_name, false).unwrap();
        let live = try_lock(lease).unwrap().unwrap();
        assert!(
            try_lock(checked_file(&directory.dir, &lease_name, false).unwrap())
                .unwrap()
                .is_none()
        );
        assert!(reserved(&directory.dir, fixture.path()).is_err());

        let name = format!("arany-tools-{}", uuid::Uuid::now_v7());
        let selected = open_directory(&fixture.path().join(&name), true).unwrap();
        selected.dir.create_dir("workspace").unwrap();
        selected
            .dir
            .write("workspace/private", b"synthetic historical payload")
            .unwrap();
        let identity = crate::tools::fs::identity(&selected.dir).unwrap();
        parent.dir.write("unrelated", b"preserve").unwrap();

        parent.dir.rename(&name, &parent.dir, "held").unwrap();
        let replacement = open_directory(&fixture.path().join(&name), true).unwrap();
        replacement.dir.write("unrelated", b"replacement").unwrap();
        assert_ne!(
            crate::tools::fs::identity(&replacement.dir).unwrap(),
            identity
        );
        assert_eq!(replacement.dir.read("unrelated").unwrap(), b"replacement");
        parent
            .dir
            .rename(&name, &parent.dir, "replacement")
            .unwrap();
        parent.dir.rename("held", &parent.dir, &name).unwrap();

        drop(live);
        let _dead = try_lock(checked_file(&directory.dir, &lease_name, false).unwrap())
            .unwrap()
            .unwrap();
        let _namespace = namespace_lock(&directory.dir).unwrap().unwrap();
        let checked = cap_std::fs::Dir::from_std_file(
            crate::tools::fs::open_file(&parent.dir, &name).unwrap(),
        );
        assert_eq!(crate::tools::fs::identity(&checked).unwrap(), identity);
        let mut remaining = SEED_ENTRY_LIMIT;
        remove_seed_contents(&checked, &mut remaining, 0).unwrap();
        let named = parent.dir.symlink_metadata(&name).unwrap();
        assert_eq!((named.dev(), named.ino()), identity);
        parent.dir.remove_dir(&name).unwrap();
        sync_directory(&parent.dir).unwrap();
        remove(&directory.dir, &lease_name).unwrap();
        sync_directory(&directory.dir).unwrap();

        assert!(!fixture.path().join(&name).exists());
        assert!(
            open_file(&directory.dir, &lease_name, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(parent.dir.read("unrelated").unwrap(), b"preserve");
        assert_eq!(replacement.dir.read("unrelated").unwrap(), b"replacement");
        let retained = limits(&directory.dir).unwrap();
        assert_eq!(
            (retained.units, retained.queued),
            (settings.units, settings.queued)
        );
    }

    #[test]
    #[ignore = "helper-only: activated by the shared admission owner"]
    fn admission_process() {
        let path = std::env::var_os("ARANY_TEST_ADMISSION").expect("private admission fixture");
        let path = Path::new(&path);
        let stream = UnixStream::connect(path.join("control")).unwrap();
        let control = stream.try_clone().unwrap();
        control.set_nonblocking(true).unwrap();
        let reactor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        reactor.block_on(async {
            let mut control = tokio::net::UnixStream::from_std(control).unwrap();
            tokio::io::AsyncReadExt::read_exact(&mut control, &mut [0])
                .await
                .unwrap();
            let directory = open_directory(&path.join("guard"), true).unwrap();
            let expiry = std::env::var("ARANY_TEST_EXPIRY").unwrap().parse().unwrap();
            let acquire = acquire_in(
                &directory,
                path,
                uuid::Uuid::now_v7(),
                expiry,
                |queued| {
                    if queued {
                        let mut writer = &stream;
                        writer.write_all(b"Q\n").unwrap();
                    }
                },
                crate::tools::now_ms,
            );
            tokio::pin!(acquire);
            let mut signal = [0];
            let result = tokio::select! {
                result = &mut acquire => result,
                _ = tokio::io::AsyncReadExt::read_exact(&mut control, &mut signal) => {
                    return;
                }
            };
            match result {
                Ok(permit) => {
                    (&stream)
                        .write_all(format!("A {}\n", permit.id).as_bytes())
                        .unwrap();
                    tokio::io::AsyncReadExt::read_exact(&mut control, &mut [0])
                        .await
                        .unwrap();
                    drop(permit);
                }
                Err(ToolError::QueueFull) => (&stream).write_all(b"F\n").unwrap(),
                Err(ToolError::Limit) => (&stream).write_all(b"E\n").unwrap(),
                Err(error) => panic!("admission: {error}"),
            }
        });
    }

    struct Participant {
        child: Child,
        control: BufReader<tokio::net::UnixStream>,
        id: Option<uuid::Uuid>,
        errors: PathBuf,
    }

    impl Participant {
        async fn message(&mut self) -> Vec<u8> {
            let message = tokio::time::timeout(
                Duration::from_secs(10),
                super::super::read_line_capped(&mut self.control, 64),
            )
            .await
            .unwrap()
            .unwrap_or_else(|error| {
                let mut diagnostic = String::new();
                std::fs::File::open(&self.errors)
                    .unwrap()
                    .take(8192)
                    .read_to_string(&mut diagnostic)
                    .unwrap();
                panic!(
                    "admission participant {}: {error}; {diagnostic}",
                    self.child.id()
                );
            });
            let message = message.strip_suffix(b"\n").unwrap();
            if let Some(id) = message.strip_prefix(b"A ") {
                self.id = Some(uuid::Uuid::parse_str(std::str::from_utf8(id).unwrap()).unwrap());
                b"A".to_vec()
            } else {
                message.to_vec()
            }
        }

        async fn release(&mut self) {
            tokio::io::AsyncWriteExt::write_all(self.control.get_mut(), b"R")
                .await
                .unwrap();
            let observed =
                crate::test_process::capture(&mut self.child, Duration::from_secs(5), 4096);
            assert!(observed.status.success());
            assert!(
                String::from_utf8_lossy(&observed.stdout).contains("test result: ok. 1 passed;"),
                "exactly one admission helper must complete"
            );
        }
    }

    impl Drop for Participant {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn participant(
        path: &Path,
        listener: &tokio::net::UnixListener,
        expiry: u64,
        begin: bool,
    ) -> Participant {
        let errors = path.join(format!("{}.errors", uuid::Uuid::now_v7()));
        let stderr = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&errors)
            .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", HELPER, "--nocapture"])
            .env_clear()
            .env("ARANY_TEST_ADMISSION", path)
            .env("ARANY_TEST_EXPIRY", expiry.to_string())
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stream = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap()
            .0;
        let mut participant = Participant {
            child,
            control: BufReader::new(stream),
            id: None,
            errors,
        };
        if begin {
            tokio::io::AsyncWriteExt::write_all(participant.control.get_mut(), b"S")
                .await
                .unwrap();
        }
        participant
    }

    #[tokio::test]
    async fn shared_capacity_and_fifo_survive_cancellation_death_and_expiry() {
        let fixture = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(fixture.path().join("control")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let expiry = crate::tools::now_ms() + 60_000;
        let mut active = Vec::new();
        for _ in 0..8 {
            active.push(participant(fixture.path(), &listener, expiry, false).await);
        }
        for owner in &mut active {
            tokio::io::AsyncWriteExt::write_all(owner.control.get_mut(), b"S")
                .await
                .unwrap();
        }
        for owner in &mut active {
            assert_eq!(owner.message().await, b"A");
        }
        let directory = open_directory(&fixture.path().join("guard"), false).unwrap();
        assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 8);
        #[cfg(target_os = "macos")]
        let (retained, root_lease) = {
            let root = fixture
                .path()
                .join(format!("arany-u-{}", active.last().unwrap().id.unwrap()));
            let root = open_directory(&root, true).unwrap();
            let lease = try_lock(checked_file(&root.dir, "unit.lock", true).unwrap())
                .unwrap()
                .unwrap();
            write_record(
                &root.dir,
                "recovery.json",
                &serde_json::json!({
                    "version":1, "root":crate::tools::fs::identity(&root.dir).unwrap(),
                    "boot":uuid::Uuid::now_v7(), "supervisor":null,
                }),
            )
            .unwrap();
            assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 8);
            (root, lease)
        };
        let mut queue = Vec::new();
        for _ in 0..32 {
            let mut waiter = participant(fixture.path(), &listener, expiry, true).await;
            assert_eq!(waiter.message().await, b"Q");
            queue.push(waiter);
        }
        let mut overflow = participant(fixture.path(), &listener, expiry, true).await;
        assert_eq!(overflow.message().await, b"F");
        assert_eq!(
            waiting(&directory.dir, crate::tools::now_ms())
                .unwrap()
                .len(),
            32
        );
        queue.remove(0).child.kill().unwrap();
        active.pop().unwrap().release().await;
        #[cfg(target_os = "macos")]
        drop(root_lease);
        assert_eq!(queue[0].message().await, b"A");
        #[cfg(target_os = "macos")]
        assert!(
            !retained.path.exists(),
            "the FIFO head recovers a root abandoned during its wait"
        );
        assert_eq!(reserved(&directory.dir, fixture.path()).unwrap(), 8);
        queue[1].release().await;
        let mut newcomer = participant(fixture.path(), &listener, expiry, true).await;
        assert_eq!(newcomer.message().await, b"Q");
        queue[0].release().await;
        assert_eq!(queue[2].message().await, b"A");
        for owner in queue.drain(3..) {
            drop(owner);
        }
        drop(newcomer);
        queue[2].release().await;
        for owner in &mut active {
            owner.release().await;
        }
        let lock = try_lock(checked_file(&directory.dir, "admission.lock", true).unwrap())
            .unwrap()
            .unwrap();
        assert!(
            waiting(&directory.dir, crate::tools::now_ms())
                .unwrap()
                .is_empty()
        );
        replace_record(
            &directory.dir,
            "limits.json",
            &Limits {
                version: 1,
                units: 1,
                queued: 1,
            },
        )
        .unwrap();
        drop(lock);
        let permit = acquire_in(
            &directory,
            fixture.path(),
            uuid::Uuid::now_v7(),
            expiry,
            |_| {},
            crate::tools::now_ms,
        )
        .await
        .unwrap();
        let clock = std::cell::Cell::new(crate::tools::now_ms());
        let (queued, ready) = tokio::sync::oneshot::channel();
        let queued = std::cell::RefCell::new(Some(queued));
        let mut expired = Box::pin(acquire_in(
            &directory,
            fixture.path(),
            uuid::Uuid::now_v7(),
            expiry,
            |is_queued| {
                if is_queued {
                    queued.borrow_mut().take().unwrap().send(()).unwrap();
                }
            },
            || clock.get(),
        ));
        tokio::select! {
            result = &mut expired => panic!("admission completed before queued readiness: {}", result.is_ok()),
            result = ready => result.unwrap(),
        }
        clock.set(expiry);
        tokio::time::pause();
        tokio::time::advance(POLL_INTERVAL).await;
        assert!(matches!(expired.await, Err(ToolError::Limit)));
        assert!(
            waiting(&directory.dir, crate::tools::now_ms())
                .unwrap()
                .is_empty()
        );
        drop(permit);
    }

    #[test]
    fn shared_records_reject_unsafe_configuration_and_reap_unpublished_leases() {
        let fixture = tempfile::tempdir().unwrap();
        let directory = open_directory(&fixture.path().join("guard"), true).unwrap();
        for (units, queued) in [(0, 32), (65, 32), (8, 0), (8, 1025)] {
            replace_record(
                &directory.dir,
                "limits.json",
                &Limits {
                    version: 1,
                    units,
                    queued,
                },
            )
            .unwrap();
            assert!(limits(&directory.dir).is_err());
        }
        let name = format!("{}.unit.new", uuid::Uuid::now_v7());
        let file = checked_file(&directory.dir, &name, true).unwrap();
        let lease = try_lock(file).unwrap().unwrap();
        reap_publications(&directory.dir).unwrap();
        assert!(directory.dir.symlink_metadata(&name).is_ok());
        drop(lease);
        reap_publications(&directory.dir).unwrap();
        assert!(directory.dir.symlink_metadata(&name).is_err());
        assert!(
            open_file(&directory.dir, "gone.wait", false)
                .unwrap()
                .is_none()
        );
        let id = uuid::Uuid::now_v7();
        let name = format!("{id}.unit");
        write_record(
            &directory.dir,
            &name,
            &Reservation {
                version: 1,
                id,
                boot: uuid::Uuid::nil(),
            },
        )
        .unwrap();
        assert!(reserved(&directory.dir, fixture.path()).is_err());
        assert!(directory.dir.symlink_metadata(&name).is_ok());
    }
}

use crate::private_fs::{self, PrivateDirectory};
use cap_std::fs::Dir;
use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
static INVENTORY: Mutex<()> = Mutex::new(());
type Identity = (u64, u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageStatus {
    Complete,
    Partial,
    Absent,
    Busy,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageUsage {
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
    pub directories: u64,
}

impl StorageUsage {
    fn add(&mut self, other: Self) -> Result<(), &'static str> {
        let next = Self {
            allocated_bytes: self
                .allocated_bytes
                .checked_add(other.allocated_bytes)
                .ok_or("byte count overflow")?,
            logical_bytes: self
                .logical_bytes
                .checked_add(other.logical_bytes)
                .ok_or("byte count overflow")?,
            files: self
                .files
                .checked_add(other.files)
                .ok_or("count overflow")?,
            directories: self
                .directories
                .checked_add(other.directories)
                .ok_or("count overflow")?,
        };
        *self = next;
        Ok(())
    }
}

#[derive(Debug)]
pub struct StorageCategory {
    pub name: &'static str,
    pub path: Option<PathBuf>,
    pub status: StorageStatus,
    pub usage: StorageUsage,
    pub reason: Option<&'static str>,
}

impl StorageCategory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            path: None,
            status: StorageStatus::Complete,
            usage: StorageUsage::default(),
            reason: None,
        }
    }

    fn partial(&mut self, reason: &'static str) {
        self.status = StorageStatus::Partial;
        self.reason = Some(reason);
    }
}

#[derive(Debug)]
pub struct StorageReport {
    pub categories: Vec<StorageCategory>,
    pub subtotal: StorageUsage,
}

impl StorageReport {
    pub fn complete(&self) -> bool {
        self.categories
            .iter()
            .all(|row| matches!(row.status, StorageStatus::Complete | StorageStatus::Absent))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    Cancelled,
    Busy,
    WorkspaceUnavailable,
}

struct Scan<'a> {
    cancelled: &'a AtomicBool,
    deadline: Instant,
    remaining: usize,
    index_bytes: usize,
    seen: BTreeSet<Identity>,
    roots: BTreeSet<Identity>,
    workspace: Identity,
}

impl Scan<'_> {
    fn check(&self) -> Result<(), &'static str> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err("cancelled");
        }
        if Instant::now() >= self.deadline {
            return Err("scan deadline reached");
        }
        Ok(())
    }

    fn entry(&mut self, path_bytes: usize) -> Result<(), &'static str> {
        self.check()?;
        self.remaining = self.remaining.checked_sub(1).ok_or("entry limit reached")?;
        self.index_bytes = self
            .index_bytes
            .checked_add(path_bytes + std::mem::size_of::<Identity>() + 64)
            .filter(|bytes| *bytes <= MAX_INDEX_BYTES)
            .ok_or("identity/path index limit reached")?;
        Ok(())
    }

    fn account(
        &mut self,
        metadata: &Metadata,
        row: &mut StorageCategory,
    ) -> Result<(), &'static str> {
        if self.seen.contains(&identity(metadata)) {
            return Ok(());
        }
        let usage = StorageUsage {
            allocated_bytes: metadata
                .blocks()
                .checked_mul(512)
                .ok_or("byte count overflow")?,
            logical_bytes: metadata.len(),
            files: u64::from(metadata.is_file()),
            directories: u64::from(metadata.is_dir()),
        };
        row.usage.add(usage)?;
        self.seen.insert(identity(metadata));
        Ok(())
    }

    fn walk(
        &mut self,
        dir: &Dir,
        row: &mut StorageCategory,
        cache: &mut StorageCategory,
        depth: usize,
        path_bytes: usize,
        cache_root: bool,
    ) -> Result<(), &'static str> {
        self.check()?;
        let before = dir_metadata(dir)?;
        private_fs::check_directory(dir, true).map_err(|_| "unsafe private directory")?;
        self.account(&before, if cache_root { cache } else { row })?;
        let entries = dir
            .entries()
            .map_err(|_| "directory metadata unavailable")?;
        let mut cache_tokens = BTreeSet::new();
        for entry in entries {
            self.entry(path_bytes)?;
            let name = entry
                .map_err(|_| "directory changed or unavailable")?
                .file_name();
            let next_bytes = path_bytes
                .checked_add(name.len() + 1)
                .filter(|bytes| *bytes <= MAX_PATH_BYTES)
                .ok_or("path limit reached")?;
            // Guard records and backing storage have a separate ownership collector.
            if depth == 0 && name == "guard" && !cache_root {
                continue;
            }
            let is_cache = depth == 0 && name == super::build_cache::DIRECTORY && !cache_root;
            let result = (|| {
                self.check()?;
                let object = metadata_file(dir, Path::new(&name), false)?;
                let metadata = object.metadata().map_err(|_| "metadata unavailable")?;
                if identity(&metadata) == self.workspace {
                    return Ok(());
                }
                if self.roots.contains(&identity(&metadata)) {
                    return Ok(());
                }
                if metadata.dev() != before.dev() {
                    return Err("unowned mount excluded");
                }
                check_private(&object, &metadata)?;
                if metadata.is_dir() {
                    if cache_root {
                        return Err("unexpected cache object");
                    }
                    if depth >= MAX_DEPTH {
                        return Err("depth limit reached");
                    }
                    let opened = metadata_file(dir, Path::new(&name), true)?;
                    let current = opened
                        .metadata()
                        .map_err(|_| "directory metadata unavailable")?;
                    if !same(&metadata, &current) {
                        return Err("storage changed during scan");
                    }
                    let child = Dir::from_std_file(opened);
                    if is_cache {
                        cache.path = row
                            .path
                            .as_ref()
                            .map(|path| path.join(super::build_cache::DIRECTORY));
                        self.walk(&child, row, cache, 0, next_bytes, true)?;
                    } else {
                        self.walk(&child, row, cache, depth + 1, next_bytes, false)?;
                    }
                } else if metadata.is_file() {
                    if cache_root {
                        let name = name.to_str().ok_or("unknown cache record")?;
                        let (token, pending) = super::build_cache::inventory_record(name)
                            .ok_or("unknown cache record")?;
                        if let Some(token) = token
                            && !cache_tokens.contains(token)
                        {
                            if cache_tokens.len() == super::build_cache::MAX_IDENTITIES {
                                return Err("cache identity limit reached");
                            }
                            cache_tokens.insert(token.to_owned());
                        }
                        if pending && cache.status == StorageStatus::Complete {
                            cache.status = StorageStatus::Busy;
                            cache.reason = Some(
                                "pending or active cache data retained; no recovery attempted",
                            );
                        }
                    }
                    self.account(&metadata, if cache_root { cache } else { row })?;
                    self.check()?;
                    let current = metadata_file(dir, Path::new(&name), false)?
                        .metadata()
                        .map_err(|_| "metadata unavailable")?;
                    if !same(&metadata, &current) {
                        return Err("storage changed during scan");
                    }
                } else {
                    return Err("unsafe private object excluded");
                }
                Ok(())
            })();
            if let Err(reason) = result {
                if matches!(
                    reason,
                    "cache identity limit reached"
                        | "byte count overflow"
                        | "count overflow"
                        | "depth limit reached"
                ) {
                    return Err(reason);
                }
                if cache_root || is_cache {
                    cache.partial(reason);
                } else {
                    row.partial(reason);
                }
                self.check()?;
            }
        }
        self.check()?;
        if !same(&before, &dir_metadata(dir)?) {
            return Err("storage changed during scan");
        }
        Ok(())
    }
}

pub fn collect_storage(
    workspace: &Path,
    selected: &Path,
    default: &Path,
    account: &Path,
    cancelled: &AtomicBool,
) -> Result<StorageReport, StorageError> {
    let _inventory = INVENTORY.try_lock().map_err(|_| StorageError::Busy)?;
    collect(
        workspace,
        selected,
        default,
        account,
        cancelled,
        MAX_ENTRIES,
        DEADLINE,
    )
}

fn collect(
    workspace: &Path,
    selected: &Path,
    default: &Path,
    account: &Path,
    cancelled: &AtomicBool,
    entries: usize,
    timeout: Duration,
) -> Result<StorageReport, StorageError> {
    let mut scan = Scan {
        cancelled,
        deadline: Instant::now() + timeout,
        remaining: entries,
        index_bytes: 0,
        seen: BTreeSet::new(),
        roots: BTreeSet::new(),
        workspace: (0, 0),
    };
    if cancelled.load(Ordering::Acquire) {
        return Err(StorageError::Cancelled);
    }
    if workspace.as_os_str().len() > MAX_PATH_BYTES || workspace.components().count() > MAX_DEPTH {
        return Err(StorageError::WorkspaceUnavailable);
    }
    let workspace = private_fs::open_workspace_directory(workspace)
        .map_err(|_| StorageError::WorkspaceUnavailable)?;
    scan.workspace =
        identity(&dir_metadata(&workspace).map_err(|_| StorageError::WorkspaceUnavailable)?);
    let mut opened: Vec<(StorageCategory, Option<PrivateDirectory>)> = Vec::new();
    for (name, path) in [
        ("Selected StateRoot", selected),
        ("Default StateRoot", default),
        ("Account root", account),
    ] {
        let mut row = StorageCategory::new(name);
        let directory = if scan.check().is_err() {
            row.partial("scan deadline reached");
            None
        } else if path.as_os_str().len() > MAX_PATH_BYTES || path.components().count() > MAX_DEPTH {
            row.partial("root path limit reached");
            None
        } else {
            match private_fs::open_directory_checked(path, false, || {
                scan.check().map_err(|_| {
                    private_fs::Error::Io(std::io::Error::from(std::io::ErrorKind::Interrupted))
                })
            }) {
                Ok(directory) => {
                    if directory.ancestors.contains(&scan.workspace) {
                        row.status = StorageStatus::Unavailable;
                        row.reason = Some("Workspace overlap excluded");
                        None
                    } else if let Ok(metadata) = dir_metadata(&directory.dir) {
                        if scan.roots.insert(identity(&metadata)) {
                            row.path = Some(directory.path.clone());
                            Some(directory)
                        } else {
                            row.reason = Some("same admitted root counted once");
                            None
                        }
                    } else {
                        row.status = StorageStatus::Unavailable;
                        row.reason = Some("root metadata unavailable");
                        None
                    }
                }
                Err(private_fs::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    row.status = StorageStatus::Absent;
                    None
                }
                Err(_) if scan.check().is_err() => {
                    row.partial("scan deadline reached");
                    None
                }
                Err(_) => {
                    row.status = StorageStatus::Unavailable;
                    row.reason = Some("root unsafe or inaccessible");
                    None
                }
            }
        };
        opened.push((row, directory));
    }
    let mut categories = Vec::new();
    let mut subtotal = StorageUsage::default();
    for (mut row, directory) in opened {
        let mut cache = StorageCategory::new("Private build cache");
        cache.status = StorageStatus::Absent;
        if let Some(directory) = directory {
            cache.status = StorageStatus::Complete;
            let result = scan.entry(directory.path.as_os_str().len()).and_then(|()| {
                scan.walk(
                    &directory.dir,
                    &mut row,
                    &mut cache,
                    0,
                    directory.path.as_os_str().len(),
                    false,
                )
            });
            if let Err(reason) = result {
                row.partial(reason);
            }
            if cache.path.is_none() && cache.status == StorageStatus::Complete {
                cache.status = StorageStatus::Absent;
            }
            match private_fs::open_directory_checked(&directory.path, false, || {
                scan.check().map_err(|_| {
                    private_fs::Error::Io(std::io::Error::from(std::io::ErrorKind::Interrupted))
                })
            }) {
                Ok(current)
                    if dir_metadata(&current.dir)
                        .ok()
                        .zip(dir_metadata(&directory.dir).ok())
                        .is_some_and(|(current, previous)| {
                            identity(&current) == identity(&previous)
                        }) => {}
                _ => row.partial("root changed during scan"),
            }
        }
        if subtotal.add(row.usage).is_err() || subtotal.add(cache.usage).is_err() {
            row.partial("byte count overflow");
        }
        categories.push(row);
        if cache.path.is_some() || cache.status != StorageStatus::Absent {
            categories.push(cache);
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(StorageError::Cancelled);
    }
    Ok(StorageReport {
        categories,
        subtotal,
    })
}

fn identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

fn same(left: &Metadata, right: &Metadata) -> bool {
    identity(left) == identity(right)
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.nlink() == right.nlink()
        && left.len() == right.len()
        && left.blocks() == right.blocks()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn dir_metadata(dir: &Dir) -> Result<Metadata, &'static str> {
    dir.try_clone()
        .map_err(|_| "directory metadata unavailable")?
        .into_std_file()
        .metadata()
        .map_err(|_| "directory metadata unavailable")
}

fn check_private(file: &File, metadata: &Metadata) -> Result<(), &'static str> {
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.mode() & 0o7000 != 0
        || !(metadata.is_dir() || metadata.is_file())
        || metadata.is_file() && metadata.nlink() != 1
    {
        return Err("unsafe private object excluded");
    }
    private_fs::check_acl(file, true).map_err(|_| "unsafe private ACL")
}

fn metadata_file(dir: &Dir, name: &Path, directory: bool) -> Result<File, &'static str> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{ResolveFlags, openat2};
        let access = if directory {
            OFlags::RDONLY | OFlags::DIRECTORY
        } else {
            OFlags::PATH
        };
        openat2(
            dir,
            name,
            flags | access,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        )
        .map(File::from)
        .map_err(|_| "unsafe, mounted or unavailable object excluded")
    }
    #[cfg(target_os = "macos")]
    {
        let access = if directory {
            OFlags::RDONLY | OFlags::DIRECTORY
        } else {
            OFlags::from_bits_retain(nix::libc::O_EVTONLY as _)
        };
        rustix::fs::openat(dir, name, flags | access, Mode::empty())
            .map(File::from)
            .map_err(|_| "unsafe or unavailable object excluded")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};

    fn directory(path: &Path) {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .expect("private fixture directory");
    }

    fn file(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("private fixture record");
        file.write_all(bytes).expect("synthetic record");
    }

    #[test]
    fn storage_metadata_deduplicates_nested_roots_and_excludes_workspace_without_opening_payloads()
    {
        let temp = tempfile::tempdir().expect("storage fixture");
        let state = temp.path().join("state");
        directory(&state);
        let workspace = state.join("workspace");
        directory(&workspace);
        file(&workspace.join("source"), b"workspace contents excluded");
        let account = state.join("account");
        directory(&account);
        file(
            &account.join("account-credentials.json"),
            b"PRIVATE_STORAGE_CANARY",
        );
        file(
            &state.join("events.sqlite3"),
            b"malformed database remains measurable",
        );
        let sparse_path = state.join("sparse");
        file(&sparse_path, b"");
        let sparse = std::fs::OpenOptions::new()
            .write(true)
            .open(&sparse_path)
            .expect("sparse fixture");
        sparse.set_len(16 * 1024 * 1024).expect("sparse extent");
        std::fs::set_permissions(&sparse_path, std::fs::Permissions::from_mode(0o000))
            .expect("no payload read permission");
        let cache = state.join("build-cache");
        directory(&cache);
        let token = "a".repeat(64);
        let pending = cache.join(format!("reserve-{token}.reserve"));
        file(&pending, b"unparsed pending record");
        let guard = state.join("guard");
        directory(&guard);
        file(&guard.join("excluded"), b"Guard collector deferred");
        let cancelled = AtomicBool::new(false);
        let report = collect(
            &workspace,
            &state,
            &state,
            &account,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("metadata report");
        assert!(!report.complete());
        assert_eq!(report.subtotal.files, 4);
        assert_eq!(report.subtotal.directories, 3);
        assert!(report.subtotal.logical_bytes >= 16 * 1024 * 1024);
        assert!(
            report
                .categories
                .iter()
                .any(|row| row.name == "Private build cache" && row.status == StorageStatus::Busy)
        );
        assert_eq!(
            std::fs::read(&pending).expect("retained reservation"),
            b"unparsed pending record"
        );
        assert_eq!(
            std::fs::read(state.join("events.sqlite3")).expect("unchanged history"),
            b"malformed database remains measurable"
        );
        assert!(!state.join("history.lock").exists());
        assert!(!state.join("events.sqlite3-wal").exists());
        let sparse_metadata = sparse.metadata().expect("sparse metadata");
        let expected_files = [
            &state.join("events.sqlite3"),
            &account.join("account-credentials.json"),
            &pending,
        ]
        .into_iter()
        .map(|path| std::fs::metadata(path).expect("record metadata"))
        .collect::<Vec<_>>();
        let expected_allocated = expected_files
            .iter()
            .chain([&sparse_metadata])
            .map(|metadata| metadata.blocks() * 512)
            .sum::<u64>()
            + [&state, &account, &cache]
                .into_iter()
                .map(|path| {
                    std::fs::metadata(path)
                        .expect("directory metadata")
                        .blocks()
                        * 512
                })
                .sum::<u64>();
        assert_eq!(report.subtotal.allocated_bytes, expected_allocated);
        assert_eq!(std::fs::read_dir(&cache).expect("cache records").count(), 1);
    }

    #[test]
    fn unsafe_objects_limits_absence_and_cancellation_never_mutate_or_claim_complete_zero() {
        let temp = tempfile::tempdir().expect("storage fixture");
        let workspace = temp.path().join("workspace");
        directory(&workspace);
        let state = temp.path().join("state");
        directory(&state);
        let missing = temp.path().join("absent");
        let outside = temp.path().join("outside");
        file(&outside, b"SECRET_TARGET_CANARY");
        let cancelled = AtomicBool::new(false);
        for name in ["secret\u{1b}]52;untrusted\u{7}", "plain"] {
            file(&state.join(name), b"metadata only");
        }
        symlink(&outside, state.join("symlink")).expect("hostile link");
        std::fs::hard_link(&outside, state.join("hard-link")).expect("hostile hard link");
        nix::unistd::mkfifo(
            &state.join("pipe"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("nonblocking special object");
        let report = collect(
            &workspace,
            &state,
            &state,
            &missing,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("partial metadata report");
        assert!(!report.complete());
        assert_eq!(report.subtotal.files, 2);
        assert!(
            report
                .categories
                .iter()
                .any(|row| row.status == StorageStatus::Absent)
        );
        assert_eq!(
            std::fs::read_dir(&state)
                .expect("unchanged entries")
                .count(),
            5
        );
        assert_eq!(
            std::fs::read(&outside).expect("unchanged target"),
            b"SECRET_TARGET_CANARY"
        );
        for (entries, timeout) in [(1, DEADLINE), (MAX_ENTRIES, Duration::ZERO)] {
            let report = collect(
                &workspace, &state, &state, &missing, &cancelled, entries, timeout,
            )
            .expect("limited report");
            assert!(!report.complete());
            assert!(
                report
                    .categories
                    .iter()
                    .any(|row| row.status == StorageStatus::Partial)
            );
        }
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            collect(
                &workspace,
                &state,
                &state,
                &missing,
                &cancelled,
                MAX_ENTRIES,
                DEADLINE
            )
            .err(),
            Some(StorageError::Cancelled)
        );
        assert_eq!(
            std::fs::read_dir(&state)
                .expect("unchanged cancellation entries")
                .count(),
            5
        );
        cancelled.store(false, Ordering::Release);
        let absent = collect(
            &workspace,
            &missing,
            &missing,
            &missing,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("absent roots");
        assert!(absent.complete());
        assert_eq!(absent.subtotal, StorageUsage::default());
        assert!(!missing.exists());
        let _held = INVENTORY.lock().expect("exclusive inventory fixture");
        assert_eq!(
            collect_storage(&workspace, &state, &state, &missing, &cancelled).err(),
            Some(StorageError::Busy)
        );
        let overlap = collect(
            &workspace,
            &workspace,
            &missing,
            &missing,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("refused overlap");
        assert!(!overlap.complete());
        assert_eq!(overlap.subtotal, StorageUsage::default());
    }
}

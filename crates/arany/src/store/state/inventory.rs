use crate::private_fs::{self, PrivateDirectory};
use crate::validate::MAX_PATH_BYTES;
use cap_std::fs::Dir;
use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 32;
const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
static INVENTORY: Mutex<()> = Mutex::new(());
type Identity = (u64, u64);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Category {
    Records,
    BuildCache,
    OutputSpill,
}

#[derive(Clone, Copy)]
struct Walk {
    category: Category,
    depth: usize,
    path_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageStatus {
    Complete,
    Partial,
    Absent,
    Busy,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageReason {
    Cancelled,
    Deadline,
    EntryLimit,
    IndexLimit,
    PathLimit,
    DepthLimit,
    CacheIdentityLimit,
    ByteOverflow,
    CountOverflow,
    UnsafeDirectory,
    DirectoryUnavailable,
    DirectoryChangedOrUnavailable,
    MetadataUnavailable,
    UnownedMount,
    UnexpectedCacheObject,
    Changed,
    UnknownCacheRecord,
    PendingCache,
    UnsafeObject,
    RootPathLimit,
    WorkspaceOverlap,
    DuplicateRoot,
    RootMetadataUnavailable,
    RootUnavailable,
    RootChanged,
    UnsafeAcl,
    ObjectUnavailable,
}

impl StorageReason {
    fn is_fatal(self) -> bool {
        match self {
            Self::CacheIdentityLimit
            | Self::ByteOverflow
            | Self::CountOverflow
            | Self::DepthLimit => true,
            Self::Cancelled
            | Self::Deadline
            | Self::EntryLimit
            | Self::IndexLimit
            | Self::PathLimit
            | Self::UnsafeDirectory
            | Self::DirectoryUnavailable
            | Self::DirectoryChangedOrUnavailable
            | Self::MetadataUnavailable
            | Self::UnownedMount
            | Self::UnexpectedCacheObject
            | Self::Changed
            | Self::UnknownCacheRecord
            | Self::PendingCache
            | Self::UnsafeObject
            | Self::RootPathLimit
            | Self::WorkspaceOverlap
            | Self::DuplicateRoot
            | Self::RootMetadataUnavailable
            | Self::RootUnavailable
            | Self::RootChanged
            | Self::UnsafeAcl
            | Self::ObjectUnavailable => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageUsage {
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
    pub directories: u64,
}

impl StorageUsage {
    fn add(&mut self, other: Self) -> Result<(), StorageReason> {
        let next = Self {
            allocated_bytes: self
                .allocated_bytes
                .checked_add(other.allocated_bytes)
                .ok_or(StorageReason::ByteOverflow)?,
            logical_bytes: self
                .logical_bytes
                .checked_add(other.logical_bytes)
                .ok_or(StorageReason::ByteOverflow)?,
            files: self
                .files
                .checked_add(other.files)
                .ok_or(StorageReason::CountOverflow)?,
            directories: self
                .directories
                .checked_add(other.directories)
                .ok_or(StorageReason::CountOverflow)?,
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
    pub reason: Option<StorageReason>,
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

    fn partial(&mut self, reason: StorageReason) {
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
    fn check(&self) -> Result<(), StorageReason> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(StorageReason::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(StorageReason::Deadline);
        }
        Ok(())
    }

    fn entry(&mut self, path_bytes: usize) -> Result<(), StorageReason> {
        self.check()?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(StorageReason::EntryLimit)?;
        self.index_bytes = self
            .index_bytes
            .checked_add(path_bytes + std::mem::size_of::<Identity>() + 64)
            .filter(|bytes| *bytes <= MAX_INDEX_BYTES)
            .ok_or(StorageReason::IndexLimit)?;
        Ok(())
    }

    fn account(
        &mut self,
        metadata: &Metadata,
        row: &mut StorageCategory,
    ) -> Result<(), StorageReason> {
        if self.seen.contains(&identity(metadata)) {
            return Ok(());
        }
        let usage = StorageUsage {
            allocated_bytes: metadata
                .blocks()
                .checked_mul(512)
                .ok_or(StorageReason::ByteOverflow)?,
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
        rows: &mut [StorageCategory; 3],
        walk: Walk,
    ) -> Result<(), StorageReason> {
        self.check()?;
        let before = dir_metadata(dir)?;
        private_fs::check_directory(dir, true).map_err(|_| StorageReason::UnsafeDirectory)?;
        self.account(&before, &mut rows[walk.category as usize])?;
        let entries = dir
            .entries()
            .map_err(|_| StorageReason::DirectoryUnavailable)?;
        let mut cache_tokens = BTreeSet::new();
        for entry in entries {
            self.entry(walk.path_bytes)?;
            let name = entry
                .map_err(|_| StorageReason::DirectoryChangedOrUnavailable)?
                .file_name();
            let path_bytes = walk
                .path_bytes
                .checked_add(name.len() + 1)
                .filter(|bytes| *bytes <= MAX_PATH_BYTES)
                .ok_or(StorageReason::PathLimit)?;
            // Guard records and backing storage have a separate ownership collector.
            if walk.depth == 0 && name == "guard" && walk.category == Category::Records {
                continue;
            }
            let category = if walk.depth == 0 && walk.category == Category::Records {
                if name == super::build_cache::DIRECTORY {
                    Category::BuildCache
                } else if name == super::spill::DIRECTORY {
                    Category::OutputSpill
                } else {
                    walk.category
                }
            } else {
                walk.category
            };
            let next = Walk {
                category,
                path_bytes,
                ..walk
            };
            if let Err(reason) =
                self.walk_entry(dir, Path::new(&name), rows, next, &mut cache_tokens)
            {
                rows[category as usize].partial(reason);
                if reason.is_fatal() {
                    return Err(reason);
                }
                self.check()?;
            }
        }
        self.check()?;
        if !same(&before, &dir_metadata(dir)?) {
            return Err(StorageReason::Changed);
        }
        Ok(())
    }

    fn walk_entry(
        &mut self,
        dir: &Dir,
        name: &Path,
        rows: &mut [StorageCategory; 3],
        walk: Walk,
        cache_tokens: &mut BTreeSet<String>,
    ) -> Result<(), StorageReason> {
        self.check()?;
        let object = metadata_file(dir, name, false)?;
        let metadata = object
            .metadata()
            .map_err(|_| StorageReason::MetadataUnavailable)?;
        if identity(&metadata) == self.workspace || self.roots.contains(&identity(&metadata)) {
            return Ok(());
        }
        if metadata.dev() != dir_metadata(dir)?.dev() {
            return Err(StorageReason::UnownedMount);
        }
        check_private(&object, &metadata)?;
        if walk.depth == 0 && walk.category == Category::OutputSpill && !metadata.is_dir() {
            return Err(StorageReason::UnsafeObject);
        }
        if metadata.is_dir() {
            if walk.category == Category::BuildCache && walk.depth == 1 {
                if !name.to_str().is_some_and(|name| {
                    name.starts_with(super::build_cache::TREE_PREFIX) && name.ends_with(".tree")
                }) {
                    return Err(StorageReason::UnexpectedCacheObject);
                }
                Self::cache_record(name, cache_tokens, &mut rows[walk.category as usize])?;
            }
            if walk.depth >= MAX_DEPTH {
                return Err(StorageReason::DepthLimit);
            }
            let opened = metadata_file(dir, name, true)?;
            let current = opened
                .metadata()
                .map_err(|_| StorageReason::DirectoryUnavailable)?;
            if !same(&metadata, &current) {
                return Err(StorageReason::Changed);
            }
            if walk.depth == 0 && walk.category != Category::Records {
                rows[walk.category as usize].path = rows[Category::Records as usize]
                    .path
                    .as_ref()
                    .map(|path| path.join(name));
            }
            let row = &mut rows[walk.category as usize];
            if row.status == StorageStatus::Absent {
                row.status = StorageStatus::Complete;
            }
            self.walk(
                &Dir::from_std_file(opened),
                rows,
                Walk {
                    depth: walk.depth + 1,
                    ..walk
                },
            )?;
        } else if metadata.is_file() {
            if walk.category == Category::BuildCache && walk.depth == 1 {
                Self::cache_record(name, cache_tokens, &mut rows[walk.category as usize])?;
            }
            self.account(&metadata, &mut rows[walk.category as usize])?;
            self.check()?;
            let current = metadata_file(dir, name, false)?
                .metadata()
                .map_err(|_| StorageReason::MetadataUnavailable)?;
            if !same(&metadata, &current) {
                return Err(StorageReason::Changed);
            }
        } else {
            return Err(StorageReason::UnsafeObject);
        }
        Ok(())
    }

    fn cache_record(
        name: &Path,
        tokens: &mut BTreeSet<String>,
        cache: &mut StorageCategory,
    ) -> Result<(), StorageReason> {
        let name = name.to_str().ok_or(StorageReason::UnknownCacheRecord)?;
        let (token, pending) =
            super::build_cache::inventory_record(name).ok_or(StorageReason::UnknownCacheRecord)?;
        if let Some(token) = token
            && !tokens.contains(token)
        {
            if tokens.len() == super::build_cache::MAX_IDENTITIES {
                return Err(StorageReason::CacheIdentityLimit);
            }
            tokens.insert(token.to_owned());
        }
        if pending && cache.status == StorageStatus::Complete {
            cache.status = StorageStatus::Busy;
            cache.reason = Some(StorageReason::PendingCache);
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
    let _inventory = inventory_guard(&INVENTORY)?;
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

fn inventory_guard(inventory: &Mutex<()>) -> Result<MutexGuard<'_, ()>, StorageError> {
    match inventory.try_lock() {
        Ok(guard) => Ok(guard),
        // This mutex protects no mutable data; poisoning cannot invalidate a later scan.
        Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => Err(StorageError::Busy),
    }
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
            row.partial(StorageReason::Deadline);
            None
        } else if path.as_os_str().len() > MAX_PATH_BYTES || path.components().count() > MAX_DEPTH {
            row.partial(StorageReason::RootPathLimit);
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
                        row.reason = Some(StorageReason::WorkspaceOverlap);
                        None
                    } else if let Ok(metadata) = dir_metadata(&directory.dir) {
                        if scan.roots.insert(identity(&metadata)) {
                            row.path = Some(directory.path.clone());
                            Some(directory)
                        } else {
                            row.reason = Some(StorageReason::DuplicateRoot);
                            None
                        }
                    } else {
                        row.status = StorageStatus::Unavailable;
                        row.reason = Some(StorageReason::RootMetadataUnavailable);
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
                    row.partial(StorageReason::Deadline);
                    None
                }
                Err(_) => {
                    row.status = StorageStatus::Unavailable;
                    row.reason = Some(StorageReason::RootUnavailable);
                    None
                }
            }
        };
        opened.push((row, directory));
    }
    let mut categories = Vec::new();
    let mut subtotal = StorageUsage::default();
    for (row, directory) in opened {
        let mut rows = [
            row,
            StorageCategory::new("Private build cache"),
            StorageCategory::new("Output spill"),
        ];
        rows[Category::BuildCache as usize].status = StorageStatus::Absent;
        rows[Category::OutputSpill as usize].status = StorageStatus::Absent;
        if let Some(directory) = directory {
            let path_bytes = directory.path.as_os_str().len();
            let result = scan.entry(path_bytes).and_then(|()| {
                scan.walk(
                    &directory.dir,
                    &mut rows,
                    Walk {
                        category: Category::Records,
                        depth: 0,
                        path_bytes,
                    },
                )
            });
            if let Err(reason) = result {
                rows[Category::Records as usize].partial(reason);
            }
            if let Err(reason) = recheck_root(&directory, || scan.check()) {
                rows[Category::Records as usize].partial(reason);
            }
        }
        for (index, mut row) in rows.into_iter().enumerate() {
            if subtotal.add(row.usage).is_err() {
                row.partial(StorageReason::ByteOverflow);
            }
            if index == Category::Records as usize
                || row.path.is_some()
                || row.status != StorageStatus::Absent
            {
                categories.push(row);
            }
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

fn recheck_root(
    directory: &PrivateDirectory,
    check: impl Fn() -> Result<(), StorageReason>,
) -> Result<(), StorageReason> {
    let check_reason = std::cell::Cell::new(None);
    match private_fs::open_directory_checked(&directory.path, false, || {
        check().map_err(|reason| {
            check_reason.set(Some(reason));
            private_fs::Error::Io(std::io::Error::from(std::io::ErrorKind::Interrupted))
        })
    }) {
        Ok(current)
            if dir_metadata(&current.dir)
                .ok()
                .zip(dir_metadata(&directory.dir).ok())
                .is_some_and(|(current, previous)| identity(&current) == identity(&previous)) =>
        {
            Ok(())
        }
        _ => Err(check_reason.get().unwrap_or(StorageReason::RootChanged)),
    }
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

fn dir_metadata(dir: &Dir) -> Result<Metadata, StorageReason> {
    dir.try_clone()
        .map_err(|_| StorageReason::DirectoryUnavailable)?
        .into_std_file()
        .metadata()
        .map_err(|_| StorageReason::DirectoryUnavailable)
}

fn check_private(file: &File, metadata: &Metadata) -> Result<(), StorageReason> {
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.mode() & 0o7000 != 0
        || !(metadata.is_dir() || metadata.is_file())
        || metadata.is_file() && metadata.nlink() != 1
    {
        return Err(StorageReason::UnsafeObject);
    }
    private_fs::check_acl(file, true).map_err(|_| StorageReason::UnsafeAcl)
}

fn metadata_file(dir: &Dir, name: &Path, directory: bool) -> Result<File, StorageReason> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::CLOEXEC | OFlags::NOFOLLOW;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{ResolveFlags, openat2};
        let access = if directory {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK
        } else {
            // openat2 rejects O_NONBLOCK with O_PATH.
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
        .map_err(|_| StorageReason::ObjectUnavailable)
    }
    #[cfg(target_os = "macos")]
    {
        let flags = flags | OFlags::NONBLOCK;
        let access = if directory {
            OFlags::RDONLY | OFlags::DIRECTORY
        } else {
            OFlags::from_bits_retain(nix::libc::O_EVTONLY as _)
        };
        rustix::fs::openat(dir, name, flags | access, Mode::empty())
            .map(File::from)
            .map_err(|_| StorageReason::ObjectUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::super::build_cache::MAX_IDENTITIES;
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
        #[cfg(target_os = "linux")]
        std::fs::set_permissions(&sparse_path, std::fs::Permissions::from_mode(0o000))
            .expect("no payload read permission");
        let cache = state.join("build-cache");
        directory(&cache);
        let token = "a".repeat(64);
        let pending = cache.join(format!("reserve-{token}.reserve"));
        file(&pending, b"unparsed pending record");
        let spill = state.join("spill");
        directory(&spill);
        let run = spill.join("private-run-name");
        directory(&run);
        let output = run.join("private-output-name.txt");
        file(&output, b"PRIVATE_SPILL_CANARY");
        #[cfg(target_os = "linux")]
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o000))
            .expect("spill metadata without payload read permission");
        let writer = run.join("writer.lock");
        file(&writer, b"");
        let writer_lock = File::open(&writer).expect("writer lock fixture");
        writer_lock.lock().expect("active spill writer");
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
        assert_eq!(
            report.subtotal.files, 6,
            "categories: {:?}",
            report.categories
        );
        assert_eq!(report.subtotal.directories, 5);
        let spill_row = report
            .categories
            .iter()
            .find(|row| row.name == "Output spill")
            .expect("spill category");
        assert_eq!(spill_row.status, StorageStatus::Complete);
        assert_eq!(spill_row.path.as_ref(), Some(&spill));
        assert_eq!(spill_row.usage.files, 2);
        assert_eq!(spill_row.usage.directories, 2);
        let records = report
            .categories
            .iter()
            .find(|row| row.name == "Selected StateRoot")
            .expect("records category");
        assert_eq!(records.usage.files, 2);
        assert_eq!(records.usage.directories, 1);
        assert_eq!(
            report
                .categories
                .iter()
                .filter(|row| row.name == "Output spill")
                .count(),
            1
        );
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
            &output,
            &writer,
        ]
        .into_iter()
        .map(|path| std::fs::metadata(path).expect("record metadata"))
        .collect::<Vec<_>>();
        let expected_allocated = expected_files
            .iter()
            .chain([&sparse_metadata])
            .map(|metadata| metadata.blocks() * 512)
            .sum::<u64>()
            + [&state, &account, &cache, &spill, &run]
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
        #[cfg(target_os = "macos")]
        {
            std::fs::set_permissions(&sparse_path, std::fs::Permissions::from_mode(0o000))
                .expect("unreadable macOS record");
            let report = collect(
                &workspace,
                &state,
                &state,
                &account,
                &cancelled,
                MAX_ENTRIES,
                DEADLINE,
            )
            .expect("unavailable metadata report");
            let selected = report
                .categories
                .iter()
                .find(|row| row.name == "Selected StateRoot")
                .expect("selected category");
            assert_eq!(selected.status, StorageStatus::Partial);
            assert!(selected.reason.is_some());
            assert_eq!(report.subtotal.files, 5);
            let retained = sparse.metadata().expect("retained unreadable record");
            assert_eq!(retained.len(), sparse_metadata.len());
            assert_eq!(retained.permissions().mode() & 0o777, 0);
            assert_eq!(
                std::fs::read_dir(&cache)
                    .expect("retained cache records")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn spill_rows_share_scan_bounds_across_distinct_roots() {
        let temp = tempfile::tempdir().expect("spill inventory fixture");
        let workspace = temp.path().join("workspace");
        directory(&workspace);
        let selected = temp.path().join("selected");
        let default = temp.path().join("default");
        let missing = temp.path().join("absent");
        for root in [&selected, &default] {
            directory(root);
            directory(&root.join("spill"));
            file(&root.join("spill/output.txt"), b"SPILL_METADATA_CANARY");
        }
        let cancelled = AtomicBool::new(false);
        let report = collect(
            &workspace,
            &selected,
            &default,
            &missing,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("distinct spill rows");
        assert!(report.complete());
        assert_eq!(report.subtotal.files, 2);
        assert_eq!(report.subtotal.directories, 4);
        assert_eq!(
            report
                .categories
                .iter()
                .filter(|row| row.name == "Output spill")
                .count(),
            2
        );
        for row in report
            .categories
            .iter()
            .filter(|row| row.name != "Output spill")
        {
            assert_eq!(row.usage.files, 0);
        }
        let limited = collect(
            &workspace, &selected, &default, &missing, &cancelled, 2, DEADLINE,
        )
        .expect("shared entry bound");
        assert!(!limited.complete());
        let spill = limited
            .categories
            .iter()
            .find(|row| row.name == "Output spill")
            .expect("partial spill row");
        assert_eq!(spill.status, StorageStatus::Partial);
        assert_eq!(spill.reason, Some(StorageReason::EntryLimit));
        assert_eq!(spill.usage.files, 0);
        assert_eq!(spill.usage.directories, 1);
        assert_eq!(
            std::fs::read(selected.join("spill/output.txt")).expect("retained spill"),
            b"SPILL_METADATA_CANARY"
        );
    }

    #[test]
    fn cache_identity_exhaustion_marks_its_known_subtotal_partial_without_mutation() {
        let temp = tempfile::tempdir().expect("cache inventory fixture");
        let workspace = temp.path().join("workspace");
        directory(&workspace);
        for (prefix, extension) in [("cache", "arc"), ("reserve", "reserve")] {
            let state = temp.path().join(prefix);
            directory(&state);
            let cache_path = state.join("build-cache");
            directory(&cache_path);
            for index in 0..=MAX_IDENTITIES {
                file(
                    &cache_path.join(format!("{prefix}-{index:064x}.{extension}")),
                    b"",
                );
            }
            let report = collect(
                &workspace,
                &state,
                &state,
                &state,
                &AtomicBool::new(false),
                MAX_ENTRIES,
                DEADLINE,
            )
            .expect("limited cache inventory");
            assert!(!report.complete());
            let cache = report
                .categories
                .iter()
                .find(|row| row.name == "Private build cache")
                .expect("cache category");
            assert_eq!(cache.status, StorageStatus::Partial, "{prefix}");
            assert_eq!(cache.reason, Some(StorageReason::CacheIdentityLimit));
            assert_eq!(cache.usage.files, MAX_IDENTITIES as u64);
            assert_eq!(
                std::fs::read_dir(&cache_path)
                    .expect("retained cache records")
                    .count(),
                MAX_IDENTITIES + 1
            );
        }
    }

    #[test]
    fn fatal_metadata_limits_stop_the_walk_with_partial_known_usage() {
        for reason in [
            StorageReason::ByteOverflow,
            StorageReason::CountOverflow,
            StorageReason::DepthLimit,
        ] {
            let temp = tempfile::tempdir().expect("scan limit fixture");
            let state = temp.path().join("state");
            directory(&state);
            let depth = if reason == StorageReason::DepthLimit {
                directory(&state.join("nested"));
                MAX_DEPTH
            } else {
                file(&state.join("record"), b"retained");
                0
            };
            let admitted = private_fs::open_directory(&state, false).expect("admitted root");
            let mut row = StorageCategory::new("Selected StateRoot");
            if reason == StorageReason::ByteOverflow {
                row.usage.logical_bytes =
                    u64::MAX - dir_metadata(&admitted.dir).expect("root metadata").len();
            } else if reason == StorageReason::CountOverflow {
                row.usage.files = u64::MAX;
            }
            let mut rows = [
                row,
                StorageCategory::new("Private build cache"),
                StorageCategory::new("Output spill"),
            ];
            let cancelled = AtomicBool::new(false);
            let mut scan = Scan {
                cancelled: &cancelled,
                deadline: Instant::now() + DEADLINE,
                remaining: MAX_ENTRIES,
                index_bytes: 0,
                seen: BTreeSet::new(),
                roots: BTreeSet::new(),
                workspace: (0, 0),
            };
            assert_eq!(
                scan.walk(
                    &admitted.dir,
                    &mut rows,
                    Walk {
                        category: Category::Records,
                        depth,
                        path_bytes: 0
                    }
                ),
                Err(reason)
            );
            assert_eq!(
                rows[Category::Records as usize].status,
                StorageStatus::Partial
            );
            assert_eq!(rows[Category::Records as usize].reason, Some(reason));
            assert_eq!(
                std::fs::read_dir(&state).expect("retained record").count(),
                1
            );
        }
    }

    #[test]
    fn unsafe_objects_limits_absence_and_cancellation_never_mutate_or_claim_complete_zero() {
        let inventory = Mutex::new(());
        for poisoned in [false, true] {
            if poisoned {
                assert!(
                    std::panic::catch_unwind(|| {
                        let _guard = inventory_guard(&inventory).expect("idle inventory");
                        panic!("synthetic inventory panic");
                    })
                    .is_err()
                );
                assert!(inventory.is_poisoned());
            }
            let guard = inventory_guard(&inventory).expect("available inventory after release");
            assert!(matches!(
                inventory_guard(&inventory),
                Err(StorageError::Busy)
            ));
            drop(guard);
            drop(inventory_guard(&inventory).expect("inventory available again"));
        }
        let temp = tempfile::tempdir().expect("storage fixture");
        let workspace = temp.path().join("workspace");
        directory(&workspace);
        let state = temp.path().join("state");
        directory(&state);
        let missing = temp.path().join("absent");
        let outside = temp.path().join("outside");
        file(&outside, b"SECRET_TARGET_CANARY");
        let cancelled = AtomicBool::new(false);
        let without_spill = collect(
            &workspace,
            &state,
            &state,
            &missing,
            &cancelled,
            MAX_ENTRIES,
            DEADLINE,
        )
        .expect("missing spill inventory");
        assert!(
            without_spill
                .categories
                .iter()
                .all(|row| row.name != "Output spill")
        );
        assert!(!state.join("spill").exists());
        let spill = state.join("spill");
        directory(&spill);
        file(&spill.join("retained.txt"), b"SPILL_RETAINED_CANARY");
        symlink(&outside, spill.join("linked-output")).expect("unsafe spill link");
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
        assert_eq!(
            report.subtotal.files, 3,
            "categories: {:?}",
            report.categories
        );
        let spill_row = report
            .categories
            .iter()
            .find(|row| row.name == "Output spill")
            .expect("partial spill category");
        assert_eq!(spill_row.status, StorageStatus::Partial);
        assert_eq!(spill_row.usage.files, 1);
        assert!(spill_row.reason.is_some());
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
            6
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
        let admitted = private_fs::open_directory(&state, false).expect("admitted storage root");
        assert_eq!(recheck_root(&admitted, || Ok(())), Ok(()));
        for reason in [StorageReason::Deadline, StorageReason::Cancelled] {
            let checks = std::cell::Cell::new(0);
            assert_eq!(
                recheck_root(&admitted, || {
                    checks.set(checks.get() + 1);
                    if checks.get() == 2 {
                        Err(reason)
                    } else {
                        Ok(())
                    }
                }),
                Err(reason)
            );
            assert_eq!(checks.get(), 2);
        }
        let retained = temp.path().join("retained");
        std::fs::rename(&state, &retained).expect("retain original root");
        directory(&state);
        assert_eq!(
            recheck_root(&admitted, || Ok(())),
            Err(StorageReason::RootChanged)
        );
        std::fs::remove_dir(&state).expect("remove empty replacement");
        std::fs::rename(&retained, &state).expect("restore original fixture root");
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
            6
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
        assert!(
            absent
                .categories
                .iter()
                .all(|row| row.name != "Output spill")
        );
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

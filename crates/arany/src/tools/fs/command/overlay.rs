use super::*;
use cap_std::fs::{Metadata, MetadataExt};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};

mod metadata;
#[cfg(test)]
mod tests;

#[derive(Default)]
pub(in crate::tools) struct Baseline {
    entries: BTreeMap<String, Metadata>,
    complete: BTreeSet<String>,
    selection: CommandSelection,
    masks: BTreeSet<String>,
}

impl Baseline {
    pub(in crate::tools) fn record(&mut self, path: &str, metadata: Metadata) {
        self.entries.insert(path.into(), metadata);
    }

    pub(in crate::tools) fn complete(&mut self, path: &Path) -> Result<(), ToolError> {
        self.complete
            .insert(path.to_str().ok_or(ToolError::Path)?.into());
        Ok(())
    }

    pub(in crate::tools) fn mask(&mut self, path: &Path) {
        self.masks.insert(path.to_string_lossy().into_owned());
        self.entries
            .retain(|child, _| !Path::new(child).starts_with(path));
        self.complete
            .retain(|child| !Path::new(child).starts_with(path));
    }

    #[cfg(test)]
    pub(in crate::tools) fn freeze(
        &mut self,
        lower: &Dir,
        deadline: Instant,
    ) -> Result<(), ToolError> {
        self.freeze_at(lower, "", &mut Selector::new(0, deadline))
    }

    pub(in crate::tools) fn freeze_at(
        &mut self,
        lower: &Dir,
        prefix: &str,
        selector: &mut Selector,
    ) -> Result<(), ToolError> {
        for mask in self.masks.clone() {
            self.mask(Path::new(&mask));
        }
        let calls = selector.calls();
        for directory in &self.complete {
            if ignored(&joined(prefix, directory), true, selector)? {
                continue;
            }
            let path = if directory.is_empty() {
                ".gitignore".into()
            } else {
                format!("{directory}/.gitignore")
            };
            self.check(lower, &path)?;
            if self.entries.contains_key(&path) {
                let file = open_file(lower, &path)?;
                safe_metadata(&file)?;
                let bytes = self.read_lower(lower, &path, CONTROL_BYTES)?;
                self.check(lower, &path)?;
                let selected = joined(prefix, directory);
                selector.add(&selected, &bytes)?;
                self.selection.controls.insert(
                    selected,
                    String::from_utf8(bytes).map_err(|_| ToolError::Path)?,
                );
            }
        }
        for (path, metadata) in &self.entries {
            if ignored(&joined(prefix, path), metadata.is_dir(), selector)? {
                self.selection.summary.ignored_entries += 1;
            }
        }
        self.selection.calls = selector.calls() - calls;
        Ok(())
    }

    fn read_lower(&self, lower: &Dir, path: &str, limit: usize) -> Result<Vec<u8>, ToolError> {
        self.check(lower, path)?;
        let expected = fingerprint(self.entries.get(path).ok_or(ToolError::Conflict)?);
        let file = open_file(lower, path)?;
        let matches = || {
            file.metadata()
                .map(Metadata::from_just_metadata)
                .is_ok_and(|metadata| fingerprint(&metadata) == expected)
        };
        if !matches() {
            return Err(ToolError::Conflict);
        }
        let bytes = read_regular(file.try_clone().map_err(|_| ToolError::Path)?, limit)?;
        if !matches() {
            return Err(ToolError::Conflict);
        }
        self.check(lower, path)?;
        Ok(bytes)
    }

    fn check(&self, lower: &Dir, path: &str) -> Result<(), ToolError> {
        if self
            .masks
            .iter()
            .any(|mask| Path::new(path).starts_with(mask))
        {
            return Err(ToolError::Conflict);
        }
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        if !parent.is_empty() {
            self.check(lower, parent)?;
        }
        match (self.entries.get(path), current(lower, path)?) {
            (Some(before), Some(now)) if fingerprint(before) == fingerprint(&now) => Ok(()),
            (None, None) => {
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                if self.complete.contains(parent) || self.absent_parent(parent) {
                    Ok(())
                } else {
                    Err(ToolError::Conflict)
                }
            }
            _ => Err(ToolError::Conflict),
        }
    }

    fn absent_parent(&self, path: &str) -> bool {
        if path.is_empty() || self.entries.contains_key(path) {
            return false;
        }
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        self.complete.contains(parent) || self.absent_parent(parent)
    }

    #[cfg(test)]
    pub(in crate::tools) fn capture(
        &self,
        lower: &Dir,
        upper: &Dir,
        deadline: Instant,
    ) -> Result<(CommandChangeset, CommandSelectionSummary), ToolError> {
        capture_roots(
            &[(
                self,
                lower.try_clone().map_err(|_| ToolError::Path)?,
                upper.try_clone().map_err(|_| ToolError::Path)?,
                "",
            )],
            deadline,
        )
    }
}

#[derive(Default)]
struct Totals {
    budget: CommandScanBudget,
    summary: CommandSelectionSummary,
    changes: ChangesetCapture,
    retained: usize,
}

pub(in crate::tools) fn capture_roots(
    roots: &[(&Baseline, Dir, Dir, &str)],
    deadline: Instant,
) -> Result<(CommandChangeset, CommandSelectionSummary), ToolError> {
    let mut totals = Totals::default();
    let mut selector = Selector::new(
        roots
            .iter()
            .map(|(baseline, ..)| baseline.selection.calls)
            .sum(),
        deadline,
    );
    for (baseline, ..) in roots {
        for (directory, bytes) in &baseline.selection.controls {
            selector.add(directory, bytes.as_bytes())?;
        }
    }
    for (baseline, lower, upper, prefix) in roots {
        totals.summary.ignored_entries += baseline.selection.summary.ignored_entries;
        let mut capture = Capture {
            baseline,
            lower,
            upper,
            prefix,
            selector: &mut selector,
            totals: &mut totals,
            files: BTreeMap::new(),
        };
        if metadata::validate(&pin(upper, "")?, &mut capture.totals.budget)? {
            return Err(ToolError::Path);
        }
        capture.scan()?;
        for (path, executable) in std::mem::take(&mut capture.files) {
            capture.selector.check_time()?;
            capture.file(&path, executable)?;
        }
    }
    totals.changes.files.sort_by(|a, b| a.path.cmp(&b.path));
    let changes = CommandChangeset {
        files: totals.changes.files,
    };
    if !changes.valid() {
        return Err(ToolError::Limit);
    }
    Selector::new(0, deadline).check_time()?;
    Ok((changes, totals.summary))
}

fn joined(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.into()
    } else if path.is_empty() {
        prefix.into()
    } else {
        format!("{prefix}/{path}")
    }
}

fn fingerprint(metadata: &Metadata) -> (u64, u64, u64, i64, i64, i64, i64, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.mode(),
    )
}

fn ignored(path: &str, directory: bool, selector: &mut Selector) -> Result<bool, ToolError> {
    Ok(path.split('/').any(ignored_directory) || selector.excluded(path, directory)?)
}

struct Capture<'a> {
    baseline: &'a Baseline,
    lower: &'a Dir,
    upper: &'a Dir,
    selector: &'a mut Selector,
    prefix: &'a str,
    totals: &'a mut Totals,
    files: BTreeMap<String, bool>,
}

impl Capture<'_> {
    fn scan(&mut self) -> Result<(), ToolError> {
        let mut pending = vec![String::new()];
        while let Some(path) = pending.pop() {
            self.directory(&path, &mut pending)?;
        }
        Ok(())
    }

    fn directory(&mut self, path: &str, pending: &mut Vec<String>) -> Result<(), ToolError> {
        self.selector.check_time()?;
        let file = readable(self.upper, path)?;
        let directory = Dir::from_std_file(file);
        let mut empty = true;
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            empty = false;
            self.selector.check_time()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let child = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            self.totals.budget.entry(&joined(self.prefix, &child))?;
            self.entry(&child, pending)?;
        }
        if empty && !path.is_empty() && !ignored(&joined(self.prefix, path), true, self.selector)? {
            self.totals.retained += 1;
            if self.totals.retained > COMMAND_SNAPSHOT_FILES {
                return Err(ToolError::Limit);
            }
        }
        Ok(())
    }

    fn entry(&mut self, path: &str, pending: &mut Vec<String>) -> Result<(), ToolError> {
        if !valid_relative(path, false) {
            return Err(if path.split('/').any(forbidden) {
                ToolError::FilesystemDenied
            } else {
                ToolError::Path
            });
        }
        if joined(self.prefix, path).split('/').count() > COMMAND_SNAPSHOT_DEPTH {
            return Err(ToolError::Limit);
        }
        let handle = pin(self.upper, path)?;
        let metadata = handle.metadata().map_err(|_| ToolError::Path)?;
        let whiteout = std::os::unix::fs::FileTypeExt::is_char_device(&metadata.file_type())
            && metadata.rdev() == 0;
        if metadata.mode() & 0o6000 != 0 {
            return Err(ToolError::FilesystemDenied);
        }
        if (!metadata.is_dir() && !metadata.is_file() && !whiteout)
            || metadata.is_file() && metadata.nlink() != 1
        {
            return Err(ToolError::Path);
        }
        if metadata.is_dir() || metadata.is_file() {
            drop(readable(self.upper, path)?);
        }
        let opaque = metadata::validate(&handle, &mut self.totals.budget)?;
        let directory_type = metadata.is_dir()
            || whiteout
                && self
                    .baseline
                    .entries
                    .get(path)
                    .is_some_and(Metadata::is_dir);
        let discard = ignored(&joined(self.prefix, path), directory_type, self.selector)?;
        if whiteout || opaque {
            let directory = if opaque || directory_type {
                path
            } else {
                path.rsplit_once('/').map_or("", |(parent, _)| parent)
            };
            let directory = joined(self.prefix, directory);
            if directory.is_empty() || !ignored(&directory, true, self.selector)? {
                return Err(ToolError::Path);
            }
        }
        if discard {
            self.totals.summary.discarded_outputs += 1;
        } else {
            self.baseline.check(self.lower, path)?;
            if metadata.is_dir() {
                if self
                    .baseline
                    .entries
                    .get(path)
                    .is_some_and(|entry| !entry.is_dir())
                {
                    return Err(ToolError::Path);
                }
            } else {
                if self.files.len() >= MAX_CHANGESET_FILES {
                    return Err(ToolError::Limit);
                }
                self.files.insert(path.into(), metadata.mode() & 0o111 != 0);
            }
        }
        if metadata.is_dir() {
            pending.push(path.into());
        }
        Ok(())
    }

    fn file(&mut self, path: &str, executable: bool) -> Result<(), ToolError> {
        self.totals.retained += 1;
        if self.totals.retained > COMMAND_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        let before = if let Some(metadata) = self.baseline.entries.get(path) {
            if !metadata.is_file() {
                return Err(ToolError::Path);
            }
            let bytes = self.baseline.read_lower(self.lower, path, MAX_FILE_BYTES)?;
            self.baseline.check(self.lower, path)?;
            Some(Entry {
                path: path.into(),
                content: Some(bytes),
                executable: metadata.mode() & 0o111 != 0,
            })
        } else {
            None
        };
        let content = read_regular(readable(self.upper, path)?, MAX_FILE_BYTES)?;
        self.totals.budget.content(content.len())?;
        self.baseline.check(self.lower, path)?;
        let after = FileVersion::from_bytes(&content, executable);
        // Identical copy-ups consume candidate caps until streaming comparison is introduced.
        let bytes = content.len()
            + before
                .as_ref()
                .and_then(|entry| entry.content.as_ref())
                .map_or(0, Vec::len);
        if self.totals.changes.bytes + bytes > MAX_CHANGESET_BYTES
            || self.totals.changes.files.len() >= MAX_CHANGESET_FILES
        {
            return Err(ToolError::Limit);
        }
        if before.as_ref().is_some_and(|entry| {
            entry.content.as_deref() == Some(&content) && entry.executable == executable
        }) {
            self.totals.changes.bytes += bytes;
            return Ok(());
        }
        self.totals.changes.push(
            &joined(self.prefix, path),
            before.as_ref(),
            Some(after),
            bytes,
        )
    }
}

fn current(root: &Dir, path: &str) -> Result<Option<Metadata>, ToolError> {
    match rustix::fs::openat2(
        root,
        path,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::BENEATH
            | rustix::fs::ResolveFlags::NO_SYMLINKS
            | rustix::fs::ResolveFlags::NO_MAGICLINKS
            | rustix::fs::ResolveFlags::NO_XDEV,
    ) {
        Ok(file) => File::from(file)
            .metadata()
            .map(Metadata::from_just_metadata)
            .map(Some)
            .map_err(|_| ToolError::Conflict),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(ToolError::Conflict),
    }
}

fn pin(root: &Dir, path: &str) -> Result<File, ToolError> {
    rustix::fs::openat2(
        root,
        if path.is_empty() { "." } else { path },
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::BENEATH
            | rustix::fs::ResolveFlags::NO_SYMLINKS
            | rustix::fs::ResolveFlags::NO_MAGICLINKS
            | rustix::fs::ResolveFlags::NO_XDEV,
    )
    .map(File::from)
    .map_err(|_| ToolError::Path)
}

fn readable(root: &Dir, path: &str) -> Result<File, ToolError> {
    let file = pin(root, path)?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(ToolError::Path);
    }
    let pinned = format!("/proc/self/fd/{}", file.as_raw_fd());
    let required = if metadata.is_dir() { 0o500 } else { 0o400 };
    if metadata.mode() & required != required {
        std::fs::set_permissions(
            &pinned,
            std::fs::Permissions::from_mode(metadata.mode() | required),
        )
        .map_err(|_| ToolError::Path)?;
    }
    std::fs::File::open(pinned).map_err(|_| ToolError::Path)
}

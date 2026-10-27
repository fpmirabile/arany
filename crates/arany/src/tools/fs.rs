use super::catalog::{nullable, string};
use super::config::Config;
use super::types::*;
use super::{ToolError, ToolRuntime};
use crate::session::WorkspaceIdentity;
use crate::validate::{forbidden, hex_digest, valid_digest, valid_relative};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

pub(super) mod command;
mod removal;
#[cfg(any(test, target_os = "macos"))]
pub(super) use command::prepare_command;
pub(super) use command::{CommandSelection, selected_command_changeset};

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::List { path } => valid_relative(path, true),
        ToolCall::Read {
            path,
            offset,
            limit,
        } => {
            (if path.starts_with(super::spill::READ_PREFIX) {
                super::spill::read_reference(path).is_some()
            } else {
                valid_relative(path, false)
            }) && offset.is_none_or(|n| n >= 1)
                && limit.is_none_or(|n| (1..=MAX_READ_PAGE_LINES).contains(&n))
        }
        ToolCall::Search { path, .. } => valid_relative(path, true) && super::search::valid(call),
        ToolCall::Mkdir { path } => valid_relative(path, false),
        ToolCall::Delete {
            path,
            expected_digest,
        } => valid_relative(path, false) && valid_digest(expected_digest),
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => {
            valid_relative(source, false)
                && valid_relative(destination, false)
                && !source.eq_ignore_ascii_case(destination)
                && valid_digest(expected_digest)
        }
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => {
            valid_relative(path, false)
                && content.len() <= MAX_EDIT_CONTENT_BYTES
                && expected_digest
                    .as_ref()
                    .is_none_or(|digest| valid_digest(digest))
        }
        ToolCall::Edit {
            path,
            expected_digest,
            old,
            new,
        } => {
            valid_relative(path, false)
                && valid_digest(expected_digest)
                && !old.is_empty()
                && old.len() + new.len() <= MAX_EDIT_CONTENT_BYTES
        }
        ToolCall::MultiEdit {
            file_path,
            expected_digest,
            edits,
        } => {
            valid_relative(file_path, false)
                && valid_digest(expected_digest)
                && (1..=MAX_MULTI_EDITS).contains(&edits.len())
                && edits
                    .iter()
                    .all(|edit| !edit.old_string.is_empty() && edit.old_string != edit.new_string)
                && edits
                    .iter()
                    .try_fold(0usize, |total, edit| {
                        total
                            .checked_add(edit.old_string.len())?
                            .checked_add(edit.new_string.len())
                    })
                    .is_some_and(|total| total <= MAX_EDIT_CONTENT_BYTES)
        }
        ToolCall::ApplyChangeset { source } => source.valid(),
        ToolCall::Glob { .. } => super::glob::valid(call),
        _ => false,
    }
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    let path = ("path", string());
    Some(match operation {
        "list" | "mkdir" => vec![path],
        "read" => vec![
            (
                "path",
                json!({"type":"string", "description":format!("Workspace file path, or {}<64 lowercase hex intent digest> from a Tool result; use offset and limit to page retained output.", super::spill::READ_PREFIX)}),
            ),
            (
                "offset",
                json!({"type":["integer", "null"], "description":format!("1-based start line, 1 through {}; null starts at line 1. Continue using next_line.", u32::MAX)}),
            ),
            (
                "limit",
                json!({"type":["integer", "null"], "description":format!("Number of lines, 1 through {MAX_READ_PAGE_LINES}; null reads up to {MAX_READ_PAGE_LINES} lines. Lines are numbered and cut after {MAX_READ_LINE_CHARS} characters; the rendered result budget may end the page earlier.")}),
            ),
        ],
        "search" => vec![
            path,
            (
                "query",
                json!({"type":"string", "description":"Text to find within one line, 1 through 512 UTF-8 bytes; literal unless regex is true."}),
            ),
            (
                "regex",
                json!({"type":["boolean", "null"], "description":"true reads query as Rust regex syntax (no look-around or backreferences); null or false matches it literally."}),
            ),
            (
                "include",
                json!({"type":["string", "null"], "description":"File glob relative to a directory path, or a named file's basename: *, ?, whole-segment ** and {a,b}. *.rs matches only the directory's own files; use **/*.rs for any depth. null searches every file."}),
            ),
            (
                "case_sensitive",
                json!({"type":["boolean", "null"], "description":"false ignores case; null or true matches case exactly."}),
            ),
            (
                "offset",
                json!({"type":["integer", "null"], "description":"Number of matching lines to skip; null starts at 0. Continue using next_offset."}),
            ),
            (
                "limit",
                json!({"type":["integer", "null"], "description":format!("Matching lines to return, 1 through {MAX_SEARCH_ROWS}; null returns up to {MAX_SEARCH_ROWS}. The rendered result budget may end the page earlier.")}),
            ),
        ],
        "glob" => super::glob::schema(),
        "multiedit" => vec![
            ("file_path", string()),
            (
                "expected_digest",
                json!({"type":"string", "description":"Exact lowercase 64-character SHA-256 of the existing file from Read or a file include."}),
            ),
            (
                "edits",
                json!({
                    "type":"array",
                    "description":"Apply in order against each preceding result; all replacements publish once or none. Combined old_string and new_string UTF-8 bytes across the batch must be at most 65536. Every intermediate file must fit 1 MiB; unchanged final content rejects.",
                    "items":{
                        "type":"object", "additionalProperties":false,
                        "properties":{
                            "old_string":{"type":"string", "description":"Nonempty exact literal text, different from new_string; no Read line-number prefixes."},
                            "new_string":{"type":"string", "description":"Literal replacement; empty removes only the matched bytes."},
                            "replace_all":{"type":["boolean","null"], "description":"true replaces all non-overlapping matches (at least one required); false or null requires exactly one match. Defaults to false."}
                        },
                        "required":["old_string","new_string","replace_all"]
                    }
                }),
            ),
        ],
        "delete" => vec![path, ("expected_digest", string())],
        "move" => vec![
            ("source", string()),
            ("destination", string()),
            ("expected_digest", string()),
        ],
        "write" => vec![
            path,
            ("expected_digest", nullable()),
            (
                "content",
                json!({"type":"string", "description":format!("Complete UTF-8 file content, at most {MAX_EDIT_CONTENT_BYTES} bytes.")}),
            ),
        ],
        "edit" => vec![
            path,
            (
                "expected_digest",
                json!({"type":"string", "description":"Exact lowercase 64-character SHA-256 from the file include or a successful Read. Never invent it."}),
            ),
            (
                "old",
                json!({"type":"string", "description":"Non-empty exact literal text occurring once in the current file, excluding Read line-number prefixes. To append, select a unique ending and include that ending plus the appended text in new."}),
            ),
            (
                "new",
                json!({"type":"string", "description":format!("Replacement literal text. Combined old and new UTF-8 size must be at most {MAX_EDIT_CONTENT_BYTES} bytes.")}),
            ),
        ],
        _ => return None,
    })
}

struct Entry {
    path: String,
    content: Option<Vec<u8>>,
    executable: bool,
}

pub(super) struct CommandSnapshot {
    root_identity: WorkspaceIdentity,
    entries: BTreeMap<String, Entry>,
}

#[derive(Default)]
pub(super) struct CommandScanBudget {
    bytes: usize,
    entries: usize,
    path_metadata_bytes: usize,
}

impl CommandScanBudget {
    pub(super) fn entry(&mut self, path: &str) -> Result<(), ToolError> {
        self.entries = self.entries.checked_add(1).ok_or(ToolError::Limit)?;
        self.path_metadata_bytes = self
            .path_metadata_bytes
            .checked_add(path.len())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<std::fs::Metadata>()))
            .ok_or(ToolError::Limit)?;
        if self.entries > COMMAND_SCAN_ENTRIES
            || self.path_metadata_bytes > COMMAND_PATH_METADATA_BYTES
        {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    pub(super) fn content(&mut self, bytes: usize) -> Result<(), ToolError> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(ToolError::Limit)?;
        if self.bytes > COMMAND_SNAPSHOT_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

#[cfg(test)]
mod command_budget_tests {
    use super::*;

    #[test]
    fn command_scan_budget_enforces_each_exact_ceiling() {
        let mut content = CommandScanBudget::default();
        assert!(content.content(COMMAND_SNAPSHOT_BYTES).is_ok());
        assert!(matches!(content.content(1), Err(ToolError::Limit)));

        let mut entries = CommandScanBudget {
            entries: COMMAND_SCAN_ENTRIES - 1,
            ..CommandScanBudget::default()
        };
        assert!(entries.entry("").is_ok());
        assert!(matches!(entries.entry(""), Err(ToolError::Limit)));

        let mut paths = CommandScanBudget {
            path_metadata_bytes: COMMAND_PATH_METADATA_BYTES
                - std::mem::size_of::<std::fs::Metadata>(),
            ..CommandScanBudget::default()
        };
        assert!(paths.entry("").is_ok());
        assert!(matches!(paths.entry(""), Err(ToolError::Limit)));
    }
}

pub(super) fn command_snapshot(workspace: &Path) -> Result<CommandSnapshot, ToolError> {
    let root = open_directory(workspace)?;
    check_change_directory(&root, &root)?;
    let mut builder = CommandSnapshotBuilder {
        root: &root,
        entries: BTreeMap::new(),
        budget: CommandScanBudget::default(),
        retained: 0,
    };
    builder.collect("")?;
    Ok(CommandSnapshot {
        root_identity: workspace_identity(&root)?,
        entries: builder.entries,
    })
}

struct CommandSnapshotBuilder<'a> {
    root: &'a Dir,
    entries: BTreeMap<String, Entry>,
    budget: CommandScanBudget,
    retained: usize,
}

impl CommandSnapshotBuilder<'_> {
    fn collect(&mut self, path: &str) -> Result<(), ToolError> {
        let directory = if path.is_empty() {
            self.root.try_clone().map_err(|_| ToolError::Path)?
        } else {
            Dir::from_std_file(open_file(self.root, path)?)
        };
        let mut names = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let selected = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            self.budget.entry(&selected)?;
            if !valid_relative(&selected, false) {
                return Err(if selected.split('/').any(forbidden) {
                    ToolError::FilesystemDenied
                } else {
                    ToolError::Path
                });
            }
            if selected.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
                return Err(ToolError::Limit);
            }
            names.push(selected);
        }
        names.sort();
        if names.is_empty() && !path.is_empty() {
            self.retain()?;
        }
        drop(directory);
        for selected in names {
            self.entry(selected)?;
        }
        Ok(())
    }

    fn entry(&mut self, path: String) -> Result<(), ToolError> {
        let file = open_file(self.root, &path)?;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o6000 != 0 {
                return Err(ToolError::FilesystemDenied);
            }
        }
        let entry = if metadata.is_dir() {
            drop(file);
            self.collect(&path)?;
            Entry {
                path: path.clone(),
                content: None,
                executable: false,
            }
        } else {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let cap = MAX_FILE_BYTES.min(COMMAND_SNAPSHOT_BYTES.saturating_sub(self.budget.bytes));
            let content = read_regular(file, cap)?;
            self.budget.content(content.len())?;
            self.retain()?;
            Entry {
                path: path.clone(),
                content: Some(content),
                executable,
            }
        };
        self.entries.insert(path, entry);
        Ok(())
    }

    fn retain(&mut self) -> Result<(), ToolError> {
        self.retained += 1;
        if self.retained > COMMAND_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn command_changeset(
    workspace: &Path,
    before: &CommandSnapshot,
) -> Result<CommandChangeset, ToolError> {
    let after = command_snapshot(workspace)?;
    compare_command_snapshots(before, &after)
}

#[cfg(test)]
fn compare_command_snapshots(
    before: &CommandSnapshot,
    after: &CommandSnapshot,
) -> Result<CommandChangeset, ToolError> {
    if after.root_identity != before.root_identity {
        return Err(ToolError::ChangedInput);
    }
    let mut files = Vec::new();
    let mut changed_bytes = 0usize;
    let paths: BTreeSet<_> = before.entries.keys().chain(after.entries.keys()).collect();
    for path in paths {
        let old = before.entries.get(path);
        let new = after.entries.get(path);
        if old.is_some_and(|entry| entry.content.is_none())
            && new.is_some_and(|entry| entry.content.is_some())
            || old.is_some_and(|entry| entry.content.is_some())
                && new.is_some_and(|entry| entry.content.is_none())
        {
            return Err(ToolError::Path);
        }
        if old.map(|entry| (&entry.content, entry.executable))
            == new.map(|entry| (&entry.content, entry.executable))
            || old
                .into_iter()
                .chain(new)
                .all(|entry| entry.content.is_none())
        {
            continue;
        }
        for entry in old.into_iter().chain(new) {
            changed_bytes = changed_bytes
                .checked_add(entry.content.as_ref().map_or(0, Vec::len))
                .ok_or(ToolError::Limit)?;
        }
        if changed_bytes > MAX_CHANGESET_BYTES || files.len() >= MAX_CHANGESET_FILES {
            return Err(ToolError::Limit);
        }
        let version = |entry: Option<&Entry>| {
            entry.and_then(|entry| {
                entry
                    .content
                    .as_ref()
                    .map(|content| FileVersion::from_bytes(content, entry.executable))
            })
        };
        let old = version(old);
        let new = version(new);
        if old != new {
            files.push(FileChange {
                path: path.clone(),
                before: old,
                after: new,
            });
        }
    }
    let changeset = CommandChangeset { files };
    if !changeset.valid() {
        return Err(ToolError::Limit);
    }
    Ok(changeset)
}

#[cfg(target_os = "macos")]
pub(super) fn macos_system_file(is_file: bool, uid: u32, nlink: u64, mode: u32) -> bool {
    is_file && uid == 0 && nlink == 1 && mode & 0o6022 == 0
}

pub(crate) fn open_directory(path: &Path) -> Result<Dir, ToolError> {
    crate::private_fs::open_workspace_directory(path).map_err(|_| ToolError::Path)
}

pub(crate) fn identity(dir: &Dir) -> Result<(u64, u64), ToolError> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let meta = dir.dir_metadata().map_err(|_| ToolError::Path)?;
        Ok((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(crate) fn workspace_identity(dir: &Dir) -> Result<WorkspaceIdentity, ToolError> {
    let (device, inode) = identity(dir)?;
    Ok(WorkspaceIdentity { device, inode })
}

pub(crate) fn open_absolute(path: &str) -> Result<File, ToolError> {
    if !super::config::valid_absolute(path) {
        return Err(ToolError::Path);
    }
    let path = Path::new(path);
    let parent = path.parent().ok_or(ToolError::Path)?;
    #[cfg(target_os = "macos")]
    let parent = crate::private_fs::macos_directory(
        parent,
        rustix::fs::OFlags::from_bits_retain(nix::libc::O_SEARCH as _),
    )
    .map_err(|_| ToolError::Path)?;
    #[cfg(not(target_os = "macos"))]
    let parent = open_directory(parent)?;
    // Absolute paths identify pinned resources, not user Tool targets.
    open_file(
        &parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(ToolError::Path)?,
    )
    .map_err(|error| match error {
        ToolError::NotFound => ToolError::Path,
        error => error,
    })
}

pub(super) fn open_file(root: &Dir, path: &str) -> Result<File, ToolError> {
    if path != "." && !valid_relative(path, false) {
        return Err(ToolError::Path);
    }
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
        let fd = openat2(
            root,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        )
        .map_err(|error| match error {
            rustix::io::Errno::NOENT => ToolError::NotFound,
            rustix::io::Errno::NOTDIR => not_directory_path(root, path),
            _ => ToolError::Path,
        })?;
        Ok(File::from(fd))
    }
    #[cfg(target_os = "macos")]
    {
        use rustix::fs::{Mode, OFlags, openat};
        use std::os::unix::fs::MetadataExt;
        let device = identity(root)?.0;
        let mut file = root
            .try_clone()
            .map_err(|_| ToolError::Path)?
            .into_std_file();
        for component in path.split('/') {
            if !file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
                return Err(non_directory_error(&file));
            }
            file = File::from(
                openat(
                    &file,
                    component,
                    OFlags::RDONLY
                        | OFlags::CLOEXEC
                        | OFlags::NONBLOCK
                        | OFlags::NOFOLLOW
                        | OFlags::NOCTTY,
                    Mode::empty(),
                )
                .map_err(|error| match error {
                    rustix::io::Errno::NOENT => ToolError::NotFound,
                    _ => ToolError::Path,
                })?,
            );
            if file.metadata().map_err(|_| ToolError::Path)?.dev() != device {
                return Err(ToolError::Path);
            }
        }
        Ok(file)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(ToolError::ProtectionUnavailable)
    }
}

fn non_directory_error(file: &File) -> ToolError {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.nlink() == 1)
        {
            return ToolError::NotFound;
        }
    }
    let _ = file;
    ToolError::Path
}

#[cfg(target_os = "linux")]
fn not_directory_path(root: &Dir, path: &str) -> ToolError {
    // ENOTDIR also covers special-file ancestors; those remain boundary refusals.
    for (end, _) in path.match_indices('/') {
        match open_file(root, &path[..end]) {
            Ok(file) => match file.metadata() {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return non_directory_error(&file),
                Err(_) => return ToolError::Path,
            },
            Err(_) => return ToolError::Path,
        }
    }
    ToolError::Path
}

pub(crate) fn read_regular(mut file: File, cap: usize) -> Result<Vec<u8>, ToolError> {
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_file() {
        return Err(ToolError::Path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(ToolError::Path);
        }
    }
    if metadata.len() > cap as u64 {
        return Err(ToolError::Limit);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::Operation)?;
    if bytes.len() > cap {
        return Err(ToolError::Limit);
    }
    Ok(bytes)
}

fn parent(root: &Dir, path: &str) -> Result<(Dir, String), ToolError> {
    if !valid_relative(path, false) {
        return Err(ToolError::Path);
    }
    let mut parts: Vec<_> = path.split('/').collect();
    let leaf = parts.pop().ok_or(ToolError::Path)?.to_owned();
    let file = open_file(
        root,
        &if parts.is_empty() {
            ".".into()
        } else {
            parts.join("/")
        },
    )?;
    if !file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
        return Err(non_directory_error(&file));
    }
    Ok((Dir::from_std_file(file), leaf))
}

fn create_parents(root: &Dir, path: &str) -> Result<(Dir, String), ToolError> {
    if !valid_relative(path, false) {
        return Err(ToolError::Path);
    }
    let mut parts: Vec<_> = path.split('/').collect();
    let leaf = parts.pop().ok_or(ToolError::Path)?.to_owned();
    let mut dir = root.try_clone().map_err(|_| ToolError::Operation)?;
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for part in parts {
        if let Err(error) = dir.create_dir_with(part, &builder)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(ToolError::Operation);
        }
        dir = dir.open_dir_nofollow(part).map_err(|_| ToolError::Path)?;
    }
    Ok((dir, leaf))
}

pub(super) fn put(root: &Dir, path: &str, bytes: &[u8], executable: bool) -> Result<(), ToolError> {
    let (parent, leaf) = create_parents(root, path)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = parent
        .open_with(leaf, &options)
        .map_err(|_| ToolError::Operation)?;
    file.write_all(bytes).map_err(|_| ToolError::Operation)
}

fn put_entry(root: &Dir, entry: Entry) -> Result<(), ToolError> {
    if let Some(content) = entry.content {
        put(root, &entry.path, &content, entry.executable)
    } else {
        let (parent, leaf) = create_parents(root, &entry.path)?;
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match parent.create_dir_with(&leaf, &builder) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => parent
                .open_dir_nofollow(&leaf)
                .map(|_| ())
                .map_err(|_| ToolError::Path),
            Err(_) => Err(ToolError::Operation),
        }
    }
}

pub(super) fn snapshot_into(
    workspace: &Dir,
    config: &Config,
    project: Option<&Dir>,
    resources: &Dir,
) -> Result<(), ToolError> {
    config.validate()?;
    let mut entries = Vec::new();
    let mut budget = ScanBudget::default();
    if let Some(target) = project {
        for path in &config.workspace_paths {
            if path == "." {
                collect_root(workspace, "", &mut entries, &mut budget, ALL, NONE)?;
            } else {
                collect(workspace, path, &mut entries, &mut budget, ALL, NONE)?;
            }
        }
        for entry in entries {
            put_entry(target, entry)?;
        }
    }
    let mut runtime_bytes = 0usize;
    #[cfg(target_os = "macos")]
    {
        charge_runtime(&mut runtime_bytes, super::guard::RUNTIME_COMPAT_BYTES.len())?;
        put(
            resources,
            super::guard::RUNTIME_COMPAT_FILE,
            super::guard::RUNTIME_COMPAT_BYTES,
            false,
        )?;
    }
    for program in config
        .commands
        .iter()
        .chain(config.mcp.iter().map(|server| &server.program))
    {
        let executable = read_regular(open_absolute(&program.executable)?, MAX_SNAPSHOT_BYTES)?;
        if hex_digest(&executable) != program.sha256 {
            return Err(ToolError::ChangedInput);
        }
        charge_runtime(&mut runtime_bytes, executable.len())?;
        if !super::guard::native_system_program(&program.executable)? {
            put(
                resources,
                &format!("programs/{}", program.name),
                &executable,
                true,
            )?;
        }
        for input in &program.inputs {
            let content = read_regular(open_absolute(&input.path)?, MAX_FILE_BYTES)?;
            if hex_digest(&content) != input.sha256 {
                return Err(ToolError::ChangedInput);
            }
            charge_runtime(&mut runtime_bytes, content.len())?;
            put(
                resources,
                &format!("inputs/{}/{}", program.name, input.destination),
                &content,
                false,
            )?;
        }
    }
    for skill in &config.skills {
        let root = open_directory(Path::new(&skill.directory))?;
        for (path, hash) in &skill.files {
            let content = read_regular(open_file(&root, path)?, MAX_TOOL_RESULT_BYTES)?;
            if hex_digest(&content) != *hash {
                return Err(ToolError::ChangedInput);
            }
            charge_runtime(&mut runtime_bytes, content.len())?;
            put(
                resources,
                &format!("skills/{}/{path}", skill.name),
                &content,
                false,
            )?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct ScanBudget {
    bytes: usize,
    entries: usize,
}

impl ScanBudget {
    fn entry(&mut self) -> Result<(), ToolError> {
        self.entries = self.entries.checked_add(1).ok_or(ToolError::Limit)?;
        if self.entries > MAX_SNAPSHOT_FILES * 4 {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

fn charge_runtime(total: &mut usize, count: usize) -> Result<(), ToolError> {
    *total = total.checked_add(count).ok_or(ToolError::Limit)?;
    if *total > MAX_RUNTIME_BYTES {
        return Err(ToolError::Limit);
    }
    Ok(())
}

// `keep` selects files before their content is read; `hidden` drops any path, pruning a directory, before it is opened.
pub(super) type Keep<'a> = &'a dyn Fn(&str) -> bool;
const ALL: Keep<'static> = &|_| true;
pub(super) const NONE: Keep<'static> = &|_| false;

fn collect(
    root: &Dir,
    path: &str,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
    keep: Keep,
    hidden: Keep,
) -> Result<(), ToolError> {
    if !valid_relative(path, false) || path.split('/').count() > 16 {
        return Err(ToolError::Path);
    }
    if hidden(path) {
        return Ok(());
    }
    let file = open_file(root, path)?;
    collect_open(root, path, file, output, budget, keep, hidden)
}

fn collect_open(
    root: &Dir,
    path: &str,
    file: File,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
    keep: Keep,
    hidden: Keep,
) -> Result<(), ToolError> {
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if metadata.is_dir() {
        let dir = Dir::from_std_file(file);
        let mut names = Vec::new();
        for entry in dir.entries().map_err(|_| ToolError::Path)? {
            budget.entry()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            if valid_relative(&name, false) && !hidden(&format!("{path}/{name}")) {
                names.push(name);
            }
            if names.len() + output.len() > MAX_SNAPSHOT_FILES {
                return Err(ToolError::Limit);
            }
        }
        names.sort();
        if names.is_empty() {
            if output.len() >= MAX_SNAPSHOT_FILES {
                return Err(ToolError::Limit);
            }
            output.push(Entry {
                path: path.to_owned(),
                content: None,
                executable: false,
            });
        }
        for name in names {
            collect(
                root,
                &format!("{path}/{name}"),
                output,
                budget,
                keep,
                hidden,
            )?;
        }
    } else if keep(path) {
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        let content = read_regular(file, MAX_FILE_BYTES)?;
        budget.bytes = budget
            .bytes
            .checked_add(content.len())
            .ok_or(ToolError::Limit)?;
        if budget.bytes > MAX_SNAPSHOT_BYTES || output.len() == MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        output.push(Entry {
            path: path.to_owned(),
            content: Some(content),
            executable,
        });
    }
    Ok(())
}

pub(super) fn ignored_directory(name: &str) -> bool {
    matches!(name, "target" | "node_modules" | ".venv" | "venv")
}

fn collect_root(
    root: &Dir,
    path: &str,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
    keep: Keep,
    hidden: Keep,
) -> Result<(), ToolError> {
    if !valid_relative(path, true) || path.split('/').count() > 16 {
        return Err(ToolError::Path);
    }
    if !path.is_empty() && hidden(path) {
        return Ok(());
    }
    let directory = if path.is_empty() {
        root.try_clone().map_err(|_| ToolError::Path)?
    } else {
        let file = open_file(root, path)?;
        if !file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
            return collect_open(root, path, file, output, budget, keep, hidden);
        }
        Dir::from_std_file(file)
    };
    let mut names = Vec::new();
    for entry in directory.entries().map_err(|_| ToolError::Path)? {
        budget.entry()?;
        let entry = entry.map_err(|_| ToolError::Path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_relative(&name, false)
            || ignored_directory(&name)
            || hidden(&if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            })
        {
            continue;
        }
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|_| ToolError::Path)?;
        if metadata.is_symlink() || !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        names.push((name, metadata.is_dir()));
    }
    names.sort();
    if names.is_empty() && !path.is_empty() {
        if output.len() >= MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        output.push(Entry {
            path: path.to_owned(),
            content: None,
            executable: false,
        });
    }
    for (name, is_dir) in names {
        let selected = if path.is_empty() {
            name
        } else {
            format!("{path}/{name}")
        };
        if is_dir {
            if selected.split('/').count() > 16 {
                return Err(ToolError::Limit);
            }
            let _ = open_file(root, &selected)?;
            collect_root(root, &selected, output, budget, keep, hidden)?;
        } else {
            collect(root, &selected, output, budget, keep, hidden)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn workspace_entries(
    workspace: &Path,
    folder: &str,
    prefix: &str,
) -> Result<Vec<String>, ToolError> {
    validate_mention_path(folder, true)?;
    validate_mention_path(prefix, true)?;
    let root = open_directory(workspace)?;
    let config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}))
        .map_err(|_| ToolError::Configuration)?;
    let call = ToolCall::List {
        path: if folder.is_empty() {
            ".".into()
        } else {
            folder.into()
        },
    };
    let output = super::discovery::inspect(super::discovery::Request {
        root: &root,
        call: &call,
        config: &config,
        hidden: NONE,
        result_bytes: MAX_TOOL_RESULT_BYTES,
        deadline: std::time::Instant::now() + super::selection::DISCOVERY_TIME,
        output: super::discovery::Output::Suggestions,
        cancellation: None,
    })?;
    super::discovery::SuggestionIndex::decode(&output)?.refine(folder, prefix)
}

pub(super) fn path_match(path: &str, query: &str) -> Option<u8> {
    let path = path.to_lowercase();
    let name = path.rsplit('/').next()?;
    if name == query {
        Some(0)
    } else if name.starts_with(query) {
        Some(1)
    } else if path.starts_with(query) {
        Some(2)
    } else if name.contains(query) {
        Some(3)
    } else if path.contains(query) {
        Some(4)
    } else {
        let mut letters = path.chars();
        query
            .chars()
            .all(|wanted| letters.any(|letter| letter == wanted))
            .then_some(5)
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn copy_seed() -> Result<(), ToolError> {
    let root = open_directory(Path::new("/seed/workspace"))?;
    let target = open_directory(Path::new("/workspace"))?;
    let mut entries = Vec::new();
    let mut budget = ScanBudget::default();
    for entry in root.entries().map_err(|_| ToolError::Path)? {
        budget.entry()?;
        let name = entry
            .map_err(|_| ToolError::Path)?
            .file_name()
            .into_string()
            .map_err(|_| ToolError::Path)?;
        collect(&root, &name, &mut entries, &mut budget, ALL, NONE)?;
    }
    for entry in entries {
        put_entry(&target, entry)?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(super) fn copy_command_seed() -> Result<(), ToolError> {
    let root = open_directory(Path::new("/seed/workspace"))?;
    let target = open_directory(Path::new("/workspace"))?;
    CommandSeedCopy {
        root: &root,
        target: &target,
        budget: CommandScanBudget::default(),
        retained: 0,
    }
    .directory("")
}

#[cfg(not(target_os = "macos"))]
struct CommandSeedCopy<'a> {
    root: &'a Dir,
    target: &'a Dir,
    budget: CommandScanBudget,
    retained: usize,
}

#[cfg(not(target_os = "macos"))]
impl CommandSeedCopy<'_> {
    fn directory(&mut self, path: &str) -> Result<(), ToolError> {
        let source = if path.is_empty() {
            self.root.try_clone().map_err(|_| ToolError::Path)?
        } else {
            Dir::from_std_file(open_file(self.root, path)?)
        };
        let mut children = Vec::new();
        for entry in source.entries().map_err(|_| ToolError::Path)? {
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
            self.budget.entry(&child)?;
            if !valid_relative(&child, false) {
                return Err(ToolError::Path);
            }
            if child.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
                return Err(ToolError::Limit);
            }
            children.push(child);
        }
        children.sort();
        if children.is_empty() && !path.is_empty() {
            self.retain()?;
            put_entry(
                self.target,
                Entry {
                    path: path.into(),
                    content: None,
                    executable: false,
                },
            )?;
        }
        drop(source);
        for child in children {
            self.entry(child)?;
        }
        Ok(())
    }

    fn entry(&mut self, path: String) -> Result<(), ToolError> {
        let file = open_file(self.root, &path)?;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o6000 != 0 {
                return Err(ToolError::FilesystemDenied);
            }
        }
        if metadata.is_dir() {
            drop(file);
            return self.directory(&path);
        }
        if !metadata.is_file() {
            return Err(ToolError::Path);
        }
        let cap = MAX_FILE_BYTES.min(COMMAND_SNAPSHOT_BYTES.saturating_sub(self.budget.bytes));
        let content = read_regular(file, cap)?;
        self.budget.content(content.len())?;
        self.retain()?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        put_entry(
            self.target,
            Entry {
                path,
                content: Some(content),
                executable,
            },
        )
    }

    fn retain(&mut self) -> Result<(), ToolError> {
        self.retained += 1;
        if self.retained > COMMAND_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

pub(super) fn read_page(
    mut reader: impl Read,
    offset: Option<u32>,
    limit: Option<u32>,
    result_bytes: usize,
) -> Result<String, ToolError> {
    use sha2::{Digest, Sha256};

    let start = offset.unwrap_or(1);
    let limit = limit.unwrap_or(MAX_READ_PAGE_LINES);
    if start == 0 || !(1..=MAX_READ_PAGE_LINES).contains(&limit) {
        return Err(ToolError::Operation);
    }
    let envelope = json!({"sha256":"0".repeat(64),"total_lines":MAX_SNAPSHOT_BYTES,
        "start_line":start,"end_line":u32::MAX,"next_line":u32::MAX,
        "truncated":false,"text":""})
    .to_string()
    .len();
    let budget = result_bytes.checked_sub(envelope).ok_or(ToolError::Limit)?;
    let mut text = String::new();
    let mut escaped = 0;
    let mut total_lines = 0u32;
    let mut shown = 0u32;
    let mut cut = false;
    let mut full = false;
    let mut finish_line = |line: &str, chars: usize, last_cr: bool| -> Result<(), ToolError> {
        total_lines += 1;
        if total_lines < start || full {
            return Ok(());
        }
        if shown == limit {
            full = true;
            return Ok(());
        }
        let chars = chars - usize::from(last_cr);
        let mut row = format!("{total_lines}\t");
        row.extend(line.chars().take(chars.min(MAX_READ_LINE_CHARS)));
        if chars > MAX_READ_LINE_CHARS {
            row.push_str(&format!(
                "… [line truncated to {MAX_READ_LINE_CHARS} chars]"
            ));
        }
        row.push('\n');
        let size: usize = row.chars().map(escaped_len).sum();
        if escaped + size > budget {
            if shown == 0 {
                return Err(ToolError::Limit);
            }
            full = true;
        } else {
            text.push_str(&row);
            escaped += size;
            shown += 1;
            cut |= chars > MAX_READ_LINE_CHARS;
        }
        Ok(())
    };
    let mut digest = Sha256::new();
    let mut bytes = 0;
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::with_capacity(chunk.len() + 3);
    let mut line = String::new();
    let mut chars = 0usize;
    let mut last_cr = false;
    loop {
        let count = reader.read(&mut chunk).map_err(|_| ToolError::Operation)?;
        if count == 0 {
            break;
        }
        bytes += count;
        if bytes > MAX_SNAPSHOT_BYTES {
            return Err(ToolError::Limit);
        }
        digest.update(&chunk[..count]);
        pending.extend_from_slice(&chunk[..count]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(ToolError::Operation),
        };
        for character in std::str::from_utf8(&pending[..valid])
            .map_err(|_| ToolError::Operation)?
            .chars()
        {
            if character == '\n' {
                finish_line(&line, chars, last_cr)?;
                line.clear();
                chars = 0;
                last_cr = false;
            } else {
                chars += 1;
                last_cr = character == '\r';
                if chars <= MAX_READ_LINE_CHARS + 1 {
                    line.push(character);
                }
            }
        }
        pending.drain(..valid);
    }
    if !pending.is_empty() {
        return Err(ToolError::Operation);
    }
    if chars > 0 {
        // A lone final CR is content; only CRLF is a line ending.
        finish_line(&line, chars, false)?;
    }
    let end = start - 1 + shown;
    let next = (shown > 0 && end < total_lines).then_some(end + 1);
    let output = json!({"sha256":digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),"total_lines":total_lines,
        "start_line":start,"end_line":end,"next_line":next,
        "truncated":cut || next.is_some(),"text":text}).to_string();
    if output.len() > result_bytes {
        return Err(ToolError::Limit);
    }
    Ok(output)
}

pub(super) fn inspect(
    root: &Dir,
    call: &ToolCall,
    config: &Config,
    hidden: Keep,
    result_bytes: usize,
    deadline: std::time::Instant,
) -> Result<String, ToolError> {
    match call {
        ToolCall::Read {
            path,
            offset,
            limit,
        } => {
            let file = open_file(root, path)?;
            let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
            if !metadata.is_file() {
                return Err(ToolError::Path);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(ToolError::Path);
                }
            }
            if metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
                return Err(ToolError::Limit);
            }
            read_page(file, *offset, *limit, result_bytes)
        }
        ToolCall::Search { path, .. } => {
            let mut search = super::search::Search::new(call, deadline)?;
            if hidden(path) || !config.in_workspace_scope(path) {
                return Err(ToolError::FilesystemDenied);
            }
            let file = open_file(root, path)?;
            if !file.metadata().map_err(|_| ToolError::Path)?.is_file() {
                return Err(ToolError::ChangedInput);
            }
            if search.includes(path) {
                let content = read_regular(file, MAX_FILE_BYTES)?;
                search.scan(path, &content)?;
            }
            search.finish(result_bytes)
        }
        _ => Err(ToolError::Operation),
    }
}

fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        '\0'..='\u{1f}' => 6,
        _ => character.len_utf8(),
    }
}

// Keeps a Limit, not an empty success, when matches exist but none fits.
pub(super) fn fit_rows(
    mut rows: Vec<Value>,
    mut truncated: bool,
    result_bytes: usize,
) -> Result<String, ToolError> {
    let had_rows = !rows.is_empty();
    loop {
        let output = json!({"entries":rows,"limit":64,"may_be_truncated":truncated}).to_string();
        if output.len() <= result_bytes {
            if had_rows && rows.is_empty() {
                return Err(ToolError::Limit);
            }
            return Ok(output);
        }
        rows.pop().ok_or(ToolError::Limit)?;
        truncated = true;
    }
}

pub(super) fn in_process_mutation(intent: &EffectIntent) -> bool {
    intent.target == crate::session::worker::EffectTarget::Workspace
        && intent.call.facts().family == super::catalog::Family::File
        && intent.call.facts().mutates
        && intent.enforcement_digest == FileReceipt::enforcement_digest()
}

#[derive(Clone, Copy, Default)]
enum MutationPhase {
    #[default]
    Preparing,
    Publishing,
    Expired,
}

#[derive(Clone, Default)]
pub(super) struct MutationControl {
    intent: Option<uuid::Uuid>,
    cancellation: Option<super::Cancellation>,
    deadline: Option<tokio::time::Instant>,
    phase: std::sync::Arc<std::sync::Mutex<MutationPhase>>,
    #[cfg(test)]
    removal_hook: Option<fn(&Dir, &str, removal::Phase)>,
}

impl MutationControl {
    pub(super) fn new(
        intent: uuid::Uuid,
        cancellation: super::Cancellation,
        deadline: tokio::time::Instant,
    ) -> Self {
        Self {
            intent: Some(intent),
            cancellation: Some(cancellation),
            deadline: Some(deadline),
            ..Self::default()
        }
    }

    fn sync_parent(&self, parent: &Dir) -> Result<(), ToolError> {
        crate::fault::file_parent_sync(self.intent)?;
        parent
            .try_clone()
            .map_err(|_| ToolError::Uncertain)?
            .into_std_file()
            .sync_all()
            .map_err(|_| ToolError::Uncertain)
    }

    fn check_phase(&self, phase: MutationPhase) -> Result<(), ToolError> {
        if matches!(phase, MutationPhase::Publishing) {
            return Ok(());
        }
        if matches!(phase, MutationPhase::Expired)
            || self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(ToolError::Timeout(String::new()));
        }
        if self
            .cancellation
            .as_ref()
            .is_some_and(super::Cancellation::is_cancelled)
        {
            return Err(ToolError::Cancelled);
        }
        Ok(())
    }

    fn check(&self) -> Result<(), ToolError> {
        self.check_phase(*self.phase.lock().map_err(|_| ToolError::Uncertain)?)
    }

    fn commit(&self) -> Result<(), ToolError> {
        let mut phase = self.phase.lock().map_err(|_| ToolError::Uncertain)?;
        self.check_phase(*phase)?;
        *phase = MutationPhase::Publishing;
        Ok(())
    }

    pub(super) fn expire(&self) -> ToolError {
        let Ok(mut phase) = self.phase.lock() else {
            return ToolError::Uncertain;
        };
        match *phase {
            MutationPhase::Publishing => ToolError::Uncertain,
            MutationPhase::Preparing | MutationPhase::Expired => {
                *phase = MutationPhase::Expired;
                ToolError::Timeout(String::new())
            }
        }
    }
}

pub(super) fn file_receipt(intent: &EffectIntent, output: &str) -> Result<FileReceipt, ToolError> {
    let mut receipt = FileReceipt {
        intent_digest: intent.digest(),
        before: None,
        after: None,
        executable: false,
    };
    let decode = |digest: &str| {
        let mut bytes = [0; 32];
        if !valid_digest(digest) {
            return Err(ToolError::Uncertain);
        }
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16)
                .map_err(|_| ToolError::Uncertain)?;
        }
        Ok(bytes)
    };
    match &intent.call {
        ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::MultiEdit { .. } => {
            let result: crate::session::rewind::FilePublicationResult =
                serde_json::from_str(output).map_err(|_| ToolError::Uncertain)?;
            receipt.before = result
                .before
                .as_ref()
                .map(|before| decode(&before.sha256))
                .transpose()?;
            receipt.after = Some(decode(&result.sha256)?);
            receipt.executable = result.executable;
        }
        ToolCall::Delete { .. } | ToolCall::Move { .. } => {
            let result: crate::session::rewind::RemovedFileResult =
                serde_json::from_str(output).map_err(|_| ToolError::Uncertain)?;
            receipt.before = Some(decode(&result.removed.sha256)?);
            if matches!(intent.call, ToolCall::Move { .. }) {
                receipt.after = receipt.before;
            }
            receipt.executable = result.removed.executable;
        }
        ToolCall::Mkdir { .. } | ToolCall::ApplyChangeset { .. } => {}
        _ => return Err(ToolError::Configuration),
    }
    Ok(receipt)
}

#[cfg(test)]
pub(super) fn native(
    workspace: &Path,
    call: &ToolCall,
    config: &Config,
) -> Result<String, ToolError> {
    mutate(
        &open_directory(workspace)?,
        call,
        config,
        &MutationControl::default(),
    )
}

pub(super) fn mutate(
    root: &Dir,
    call: &ToolCall,
    config: &Config,
    control: &MutationControl,
) -> Result<String, ToolError> {
    control.check()?;
    match call {
        ToolCall::Delete { .. } | ToolCall::Move { .. } => remove_or_move(root, call, control),
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => {
            let bytes = content.as_bytes();
            replace(root, path, expected_digest.as_deref(), bytes, control)
        }
        ToolCall::Mkdir { path } => {
            let (parent, leaf) = parent(root, path)?;
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt;
                builder.mode(0o755);
            }
            control.commit()?;
            parent.create_dir_with(&leaf, &builder).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ToolError::Conflict
                } else {
                    ToolError::Operation
                }
            })?;
            control.sync_parent(&parent)?;
            Ok(json!({"created":true}).to_string())
        }
        ToolCall::Edit {
            path,
            expected_digest,
            ..
        }
        | ToolCall::MultiEdit {
            file_path: path,
            expected_digest,
            ..
        } => {
            let bytes = read_regular(open_file(root, path)?, MAX_FILE_BYTES)?;
            let replacement = edited_content(&bytes, call)?;
            let bytes = replacement.as_bytes();
            replace(root, path, Some(expected_digest), bytes, control)
        }
        ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => {
            config.validate()?;
            if !config.allows(call) {
                return Err(ToolError::Path);
            }
            apply_changeset(root, changeset, control)
        }
        _ => Err(ToolError::Operation),
    }
}

pub(super) fn edited_content(bytes: &[u8], call: &ToolCall) -> Result<String, ToolError> {
    if !valid(call) {
        return Err(ToolError::Operation);
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ToolError::Limit);
    }
    let expected_digest = match call {
        ToolCall::Edit {
            expected_digest, ..
        }
        | ToolCall::MultiEdit {
            expected_digest, ..
        } => expected_digest,
        _ => return Err(ToolError::Operation),
    };
    if hex_digest(bytes) != *expected_digest {
        return Err(ToolError::Conflict);
    }
    let original = std::str::from_utf8(bytes).map_err(|_| ToolError::Operation)?;
    let replace = |text: &str, old: &str, new: &str, all: bool| {
        let count = text.matches(old).count();
        if count == 0 || !all && count != 1 {
            return Err(ToolError::Conflict);
        }
        old.len()
            .checked_mul(count)
            .and_then(|removed| text.len().checked_sub(removed))
            .and_then(|size| {
                new.len()
                    .checked_mul(count)
                    .and_then(|added| size.checked_add(added))
            })
            .filter(|size| *size <= MAX_FILE_BYTES)
            .ok_or(ToolError::Limit)?;
        Ok(text.replace(old, new))
    };
    match call {
        ToolCall::Edit { old, new, .. } => replace(original, old, new, false),
        ToolCall::MultiEdit { edits, .. } => {
            let mut result = original.to_owned();
            for edit in edits {
                result = replace(
                    &result,
                    &edit.old_string,
                    &edit.new_string,
                    edit.replace_all,
                )?;
            }
            if result == original {
                return Err(ToolError::Conflict);
            }
            Ok(result)
        }
        _ => Err(ToolError::Operation),
    }
}

fn remove_or_move(
    root: &Dir,
    call: &ToolCall,
    control: &MutationControl,
) -> Result<String, ToolError> {
    let (source, destination, expected) = match call {
        ToolCall::Delete {
            path,
            expected_digest,
        } => (path, None, expected_digest),
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => (source, Some(destination), expected_digest),
        _ => return Err(ToolError::Configuration),
    };
    let before = rewind_file(root, source)?.ok_or(ToolError::NotFound)?;
    if before.sha256 != *expected {
        return Err(ToolError::Conflict);
    }
    let removed = crate::session::rewind::FileState {
        sha256: before.sha256.clone(),
        bytes: before
            .bytes_with_limit(MAX_FILE_BYTES)
            .ok_or(ToolError::Limit)?
            .len() as u32,
        executable: before.executable,
    };
    let mut files = Vec::new();
    if let Some(destination) = destination {
        files.push(FileChange {
            path: destination.clone(),
            before: None,
            after: Some(before.clone()),
        });
    }
    files.push(FileChange {
        path: source.clone(),
        before: Some(before),
        after: None,
    });
    worker_publication_paths(root, &files)?;
    apply_files_controlled(root, &files, control)?;
    serde_json::to_string(&crate::session::rewind::RemovedFileResult { removed })
        .map_err(|_| ToolError::Uncertain)
}

struct ChangeParent {
    directory: std::sync::Arc<Dir>,
    path: String,
    missing: Vec<String>,
    leaf: String,
}

fn change_parent(root: &Dir, path: &str) -> Result<ChangeParent, ToolError> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let leaf = parts.pop().ok_or(ToolError::Path)?.to_owned();
    let mut directory = Dir::from_std_file(open_file(root, ".")?);
    let mut existing = String::new();
    for (index, part) in parts.iter().enumerate() {
        match directory.symlink_metadata(part) {
            Ok(_) => {
                let next = Dir::from_std_file(open_file(&directory, part)?);
                check_change_directory(root, &next)?;
                directory = next;
                if !existing.is_empty() {
                    existing.push('/');
                }
                existing.push_str(part);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ChangeParent {
                    directory: std::sync::Arc::new(directory),
                    path: existing,
                    missing: parts[index..]
                        .iter()
                        .map(|part| (*part).to_owned())
                        .collect(),
                    leaf,
                });
            }
            Err(_) => return Err(ToolError::Path),
        }
    }
    check_change_directory(root, &directory)?;
    Ok(ChangeParent {
        directory: std::sync::Arc::new(directory),
        path: existing,
        missing: Vec::new(),
        leaf,
    })
}

fn check_change_directory(root: &Dir, directory: &Dir) -> Result<(), ToolError> {
    let metadata = directory.dir_metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_dir() || identity(directory)?.0 != identity(root)?.0 {
        return Err(ToolError::Path);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        if metadata.mode() & 0o6000 != 0 {
            return Err(ToolError::Path);
        }
    }
    Ok(())
}

fn check_change_file(
    parent: &Dir,
    leaf: &str,
    expected: Option<&FileVersion>,
) -> Result<u32, ToolError> {
    let Some(expected) = expected else {
        return if parent
            .symlink_metadata(leaf)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            Ok(0o600)
        } else {
            Err(ToolError::Conflict)
        };
    };
    let file = open_file(parent, leaf).map_err(|_| ToolError::Conflict)?;
    let metadata = file.metadata().map_err(|_| ToolError::Conflict)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    if mode & 0o6000 != 0 {
        return Err(ToolError::Path);
    }
    let bytes = read_regular(file, MAX_FILE_BYTES)?;
    if hex_digest(&bytes) != expected.sha256 || (mode & 0o111 != 0) != expected.executable {
        return Err(ToolError::Conflict);
    }
    Ok(mode & 0o777)
}

fn verify_change_parent(root: &Dir, parent: &ChangeParent) -> Result<(), ToolError> {
    let live = Dir::from_std_file(open_file(
        root,
        if parent.path.is_empty() {
            "."
        } else {
            &parent.path
        },
    )?);
    check_change_directory(root, &live)?;
    if identity(&live)? != identity(&parent.directory)? {
        return Err(ToolError::Conflict);
    }
    Ok(())
}

fn apply_changeset(
    root: &Dir,
    changeset: &CommandChangeset,
    control: &MutationControl,
) -> Result<String, ToolError> {
    if !changeset.valid() {
        return Err(ToolError::Path);
    }
    apply_files_controlled(root, &changeset.files, control)
}

pub(super) fn apply_worker_changeset(
    workspace: &Dir,
    changeset: &crate::session::worker::WorkerChangeset,
    control: &MutationControl,
) -> Result<String, ToolError> {
    worker_publication_paths(workspace, &changeset.files)?;
    apply_files_controlled(workspace, &changeset.files, control)
}

pub(super) fn apply_files(root: &Dir, files: &[FileChange]) -> Result<String, ToolError> {
    apply_files_controlled(root, files, &MutationControl::default())
}

fn apply_files_controlled(
    root: &Dir,
    files: &[FileChange],
    control: &MutationControl,
) -> Result<String, ToolError> {
    check_change_directory(root, root)?;
    let mut parents = Vec::new();
    let mut directories = BTreeMap::<String, std::sync::Arc<Dir>>::new();
    for change in files {
        let mut parent = change_parent(root, &change.path)?;
        if let Some(directory) = directories.get(&parent.path) {
            if identity(directory)? != identity(&parent.directory)? {
                return Err(ToolError::Conflict);
            }
            parent.directory = std::sync::Arc::clone(directory);
        } else {
            directories.insert(
                parent.path.clone(),
                std::sync::Arc::clone(&parent.directory),
            );
        }
        if parent.missing.is_empty() {
            check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())?;
        } else if change.before.is_some() {
            return Err(ToolError::Conflict);
        }
        parents.push(parent);
    }
    for (change, parent) in files.iter().zip(&parents) {
        verify_change_parent(root, parent)?;
        if parent.missing.is_empty() {
            check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())?;
        } else if !parent
            .directory
            .symlink_metadata(&parent.missing[0])
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(ToolError::Conflict);
        }
    }
    let mut created = BTreeMap::new();
    let mut affected = false;
    for (change, mut parent) in files.iter().zip(parents) {
        if change.before.is_none() && change.after.is_none() {
            continue;
        }
        let result = (|| {
            verify_change_parent(root, &parent)?;
            for part in &parent.missing {
                let path = if parent.path.is_empty() {
                    part.clone()
                } else {
                    format!("{}/{part}", parent.path)
                };
                if let Some(expected) = created.get(&path) {
                    let live = Dir::from_std_file(open_file(&parent.directory, part)?);
                    check_change_directory(root, &live)?;
                    if identity(&live)? != *expected {
                        return Err(ToolError::Conflict);
                    }
                    parent.directory = std::sync::Arc::new(live);
                } else {
                    let mut builder = DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use cap_std::fs::DirBuilderExt;
                        builder.mode(0o700);
                    }
                    control.commit()?;
                    parent
                        .directory
                        .create_dir_with(part, &builder)
                        .map_err(|error| {
                            if error.kind() == std::io::ErrorKind::AlreadyExists {
                                ToolError::Conflict
                            } else {
                                ToolError::Operation
                            }
                        })?;
                    affected = true;
                    control.sync_parent(&parent.directory)?;
                    let live = Dir::from_std_file(open_file(&parent.directory, part)?);
                    check_change_directory(root, &live)?;
                    created.insert(path.clone(), identity(&live)?);
                    parent.directory = std::sync::Arc::new(live);
                }
                parent.path = path;
            }
            verify_change_parent(root, &parent)?;
            let mode = check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())?;
            if let Some(after) = &change.after {
                let bytes = after
                    .bytes_with_limit(MAX_FILE_BYTES)
                    .ok_or(ToolError::Path)?;
                let mode = mode & 0o666
                    | if after.executable {
                        if mode & 0o111 == 0 {
                            0o100
                        } else {
                            mode & 0o111
                        }
                    } else {
                        0
                    };
                replace_file(
                    &parent.directory,
                    &parent.leaf,
                    change.before.is_none(),
                    &bytes,
                    mode,
                    control,
                    || {
                        verify_change_parent(root, &parent)?;
                        check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())
                            .map(|_| ())
                    },
                )?;
            } else {
                removal::remove(root, &parent, change.before.as_ref(), control)?;
            }
            affected = true;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(if affected {
                ToolError::Uncertain
            } else {
                error
            });
        }
    }
    Ok(json!({"applied":files.len()}).to_string())
}

fn replace(
    root: &Dir,
    path: &str,
    expected: Option<&str>,
    bytes: &[u8],
    control: &MutationControl,
) -> Result<String, ToolError> {
    let (parent, leaf) = parent(root, path)?;
    let inspect = || match open_file(&parent, &leaf) {
        Ok(file) => {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                file.metadata().map_err(|_| ToolError::Operation)?.mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o600;
            let current = read_regular(file, MAX_FILE_BYTES)?;
            let sha256 = hex_digest(&current);
            if expected != Some(sha256.as_str()) {
                return Err(ToolError::Conflict);
            }
            Ok((
                Some(crate::session::rewind::FileState {
                    sha256,
                    bytes: current.len() as u32,
                    executable: mode & 0o111 != 0,
                }),
                mode,
            ))
        }
        Err(_)
            if expected.is_none()
                && parent
                    .symlink_metadata(&leaf)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok((None, 0o600))
        }
        Err(_) => Err(ToolError::Conflict),
    };
    let (before, mode) = inspect()?;
    let create = expected.is_none();
    replace_file(&parent, &leaf, create, bytes, mode, control, || {
        if inspect()? == (before.clone(), mode) {
            Ok(())
        } else {
            Err(ToolError::Conflict)
        }
    })?;
    serde_json::to_string(&crate::session::rewind::FilePublicationResult {
        sha256: hex_digest(bytes),
        bytes: bytes.len() as u32,
        executable: mode & 0o111 != 0,
        before,
    })
    .map_err(|_| ToolError::Operation)
}

fn replace_file(
    parent: &Dir,
    leaf: &str,
    create: bool,
    bytes: &[u8],
    mode: u32,
    control: &MutationControl,
    verify: impl Fn() -> Result<(), ToolError>,
) -> Result<(), ToolError> {
    control.check()?;
    let temporary = format!(".arany-write-{}", uuid::Uuid::now_v7());
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let result = (|| {
        let mut file = parent
            .open_with(&temporary, &options)
            .map_err(|_| ToolError::Operation)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| ToolError::Operation)?;
        #[cfg(unix)]
        {
            use cap_std::fs::PermissionsExt;
            file.set_permissions(cap_std::fs::Permissions::from_mode(mode))
                .map_err(|_| ToolError::Operation)?;
        }
        verify()?;
        control.commit()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if create {
                rustix::fs::renameat_with(
                    parent,
                    &temporary,
                    parent,
                    leaf,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(|_| ToolError::Conflict)?;
            } else {
                parent
                    .rename(&temporary, parent, leaf)
                    .map_err(|_| ToolError::Operation)?;
            }
            control.sync_parent(parent)?;
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(ToolError::ProtectionUnavailable)
        }
    })();
    let _ = parent.remove_file(&temporary);
    result
}

pub(super) fn validate_workspace(runtime: &ToolRuntime) -> Result<(), ToolError> {
    if workspace_identity(&open_directory(&runtime.workspace)?)? != runtime.workspace_identity {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

pub(super) fn reject_overlap(path: &Path, protected: &Path) -> Result<(), ToolError> {
    if path.starts_with(protected) || protected.starts_with(path) {
        return Err(ToolError::Path);
    }
    let protected_identity = open_directory(protected)
        .ok()
        .and_then(|dir| identity(&dir).ok());
    if let Some(protected_identity) = protected_identity {
        let mut candidate = Some(path);
        while let Some(path) = candidate {
            if let Ok(dir) = open_directory(path)
                && identity(&dir)? == protected_identity
            {
                return Err(ToolError::Path);
            }
            candidate = path.parent();
        }
    }
    Ok(())
}

pub(super) fn worker_publication_paths(root: &Dir, files: &[FileChange]) -> Result<(), ToolError> {
    let mut listings = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut scanned = 0usize;
    for change in files {
        let mut directory = root.try_clone().map_err(|_| ToolError::Path)?;
        let mut parent = String::new();
        let parts: Vec<_> = change.path.split('/').collect();
        for (index, part) in parts.iter().enumerate() {
            if !listings.contains_key(&parent) {
                let mut names = BTreeMap::new();
                for entry in directory.entries().map_err(|_| ToolError::Path)? {
                    scanned += 1;
                    if scanned > 8192 {
                        return Err(ToolError::Limit);
                    }
                    let name = entry
                        .map_err(|_| ToolError::Path)?
                        .file_name()
                        .into_string()
                        .map_err(|_| ToolError::Path)?;
                    let folded = name.to_ascii_lowercase();
                    if names.insert(folded, name).is_some() {
                        return Err(ToolError::Path);
                    }
                }
                listings.insert(parent.clone(), names);
            }
            let exact = listings[&parent].get(&part.to_ascii_lowercase());
            if exact.is_some_and(|name| name != part) {
                return Err(ToolError::Path);
            }
            if exact.is_none() {
                if directory.symlink_metadata(part).is_ok() {
                    return Err(ToolError::Path);
                }
                break;
            }
            if index + 1 < parts.len() {
                directory = Dir::from_std_file(open_file(&directory, part)?);
                check_change_directory(root, &directory)?;
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(part);
            }
        }
    }
    Ok(())
}

pub(super) fn rewind_file(root: &Dir, path: &str) -> Result<Option<FileVersion>, ToolError> {
    if !valid_relative(path, false) {
        return Err(ToolError::Path);
    }
    worker_publication_paths(
        root,
        &[FileChange {
            path: path.into(),
            before: None,
            after: None,
        }],
    )?;
    let parent = change_parent(root, path)?;
    if !parent.missing.is_empty() {
        return Ok(None);
    }
    match parent.directory.symlink_metadata(&parent.leaf) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ToolError::Path),
        Ok(_) => {
            let file = open_file(&parent.directory, &parent.leaf)?;
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .map_err(|_| ToolError::Path)?
                .permissions()
                .mode();
            if mode & 0o6000 != 0 {
                return Err(ToolError::Path);
            }
            let bytes = read_regular(file, MAX_FILE_BYTES)?;
            Ok(Some(FileVersion::from_bytes(&bytes, mode & 0o111 != 0)))
        }
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn deadline_prevents_late_publication_and_preserves_started_effects() {
        let stop = super::super::Cancellation::new();
        let interval = std::time::Duration::from_secs(1);
        let deadline = tokio::time::Instant::now() + interval;
        let preparing = MutationControl::new(uuid::Uuid::now_v7(), stop.clone(), deadline);
        let publishing = MutationControl::new(uuid::Uuid::now_v7(), stop.clone(), deadline);
        preparing.check().unwrap();
        publishing.commit().unwrap();
        tokio::time::advance(interval).await;
        assert!(matches!(preparing.commit(), Err(ToolError::Timeout(_))));
        assert!(matches!(preparing.check(), Err(ToolError::Timeout(_))));
        let temp = tempfile::tempdir().unwrap();
        let root = open_directory(temp.path()).unwrap();
        assert!(matches!(
            replace(&root, "file", None, b"content", &preparing),
            Err(ToolError::Timeout(_))
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        stop.cancel();
        assert!(publishing.check().is_ok());
        assert!(matches!(publishing.expire(), ToolError::Uncertain));
        replace(&root, "file", None, b"content", &publishing).unwrap();
        assert_eq!(std::fs::read(temp.path().join("file")).unwrap(), b"content");
    }
}

#[cfg(test)]
mod read_page_tests {
    use super::*;

    fn page(bytes: &[u8], offset: Option<u32>, limit: Option<u32>) -> Value {
        serde_json::from_str(&read_page(bytes, offset, limit, MAX_TOOL_RESULT_BYTES).unwrap())
            .unwrap()
    }

    #[test]
    fn line_pages_cover_defaults_continuations_endings_and_empty_files() {
        let content = "hello\n".repeat(10_000);
        let first = page(content.as_bytes(), None, None);
        assert_eq!(first["start_line"], 1);
        assert_eq!(first["end_line"], 2000);
        assert_eq!(first["next_line"], 2001);
        assert_eq!(first["total_lines"], 10_000);
        assert_eq!(first["sha256"], hex_digest(content.as_bytes()));
        assert!(first["text"].as_str().unwrap().starts_with("1\thello\n"));
        let next = page(content.as_bytes(), Some(2001), None);
        assert_eq!(next["end_line"], 4000);
        assert_eq!(next["next_line"], 4001);
        for (bytes, offset, text, total, end) in [
            (
                &b"a\r\nb\n\nlast"[..],
                Some(2),
                "2\tb\n3\t\n4\tlast\n",
                4,
                4,
            ),
            (&b"a\n"[..], Some(2), "", 1, 1),
            (&b"a\n"[..], Some(99), "", 1, 98),
            (&b""[..], None, "", 0, 0),
            (&b"\n"[..], None, "1\t\n", 1, 1),
            (&b"a\r"[..], None, "1\ta\r\n", 1, 1),
        ] {
            let result = page(bytes, offset, None);
            assert_eq!(result["text"], text);
            assert_eq!(result["total_lines"], total);
            assert_eq!(result["end_line"], end);
            assert_eq!(result["next_line"], Value::Null);
            assert_eq!(result["truncated"], false);
        }
        let last = page(content.as_bytes(), Some(9999), Some(2));
        assert_eq!(last["end_line"], 10_000);
        assert_eq!(last["next_line"], Value::Null);
        assert_eq!(last["truncated"], false);
    }

    #[test]
    fn streamed_reads_bound_long_lines_input_and_rendered_output() {
        for content in [
            "é".repeat(2001),
            "x".repeat(3 * 1024 * 1024),
            format!("{}é", "a".repeat(8191)),
        ] {
            let result = page(content.as_bytes(), None, None);
            assert_eq!(result["total_lines"], 1);
            assert_eq!(result["next_line"], Value::Null);
            assert_eq!(result["truncated"], true);
            assert_eq!(result["sha256"], hex_digest(content.as_bytes()));
            assert_eq!(
                result["text"],
                format!(
                    "1\t{}… [line truncated to 2000 chars]\n",
                    content.chars().take(2000).collect::<String>()
                )
            );
        }
        let crlf = format!("{}\r\n", "é".repeat(2000));
        assert_eq!(page(crlf.as_bytes(), None, None)["truncated"], false);
        let file = tempfile::tempfile().unwrap();
        let content = "123456789\n".repeat(2 * 1024 * 1024);
        (&file).write_all(content.as_bytes()).unwrap();
        use std::io::{Seek, SeekFrom};
        (&file).seek(SeekFrom::Start(0)).unwrap();
        let result: Value =
            serde_json::from_str(&read_page(file, None, None, MAX_TOOL_RESULT_BYTES).unwrap())
                .unwrap();
        assert_eq!(result["total_lines"], 2 * 1024 * 1024);
        assert_eq!(result["sha256"], hex_digest(content.as_bytes()));
        let escaped = "\"\\\t\0é\n".repeat(2000);
        let output = read_page(escaped.as_bytes(), None, None, 512).unwrap();
        assert!(output.len() <= 512);
        let result: Value = serde_json::from_str(&output).unwrap();
        assert!(result["next_line"].as_u64().unwrap() > 1);
        assert!(matches!(
            read_page(
                std::io::repeat(b'x').take(MAX_SNAPSHOT_BYTES as u64 + 1),
                None,
                None,
                MAX_TOOL_RESULT_BYTES
            ),
            Err(ToolError::Limit)
        ));
        for invalid in [vec![0xff], [b"ok\n".repeat(2000), vec![0xc3]].concat()] {
            assert!(matches!(
                read_page(invalid.as_slice(), None, None, MAX_TOOL_RESULT_BYTES),
                Err(ToolError::Operation)
            ));
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod path_tests {
    use super::*;

    #[tokio::test]
    async fn missing_paths_fail_inside_scope_while_boundary_refusals_stay_denied() {
        use super::super::types::{Attestation, ToolDisposition};
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/file"), "regular").unwrap();
        std::fs::write(workspace.join("src/aliased"), "alias").unwrap();
        std::fs::hard_link(
            workspace.join("src/aliased"),
            workspace.join("src/hardlink"),
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", workspace.join("src/link")).unwrap();
        std::os::unix::fs::symlink(".", workspace.join("src/directory-link")).unwrap();
        nix::unistd::mkfifo(&workspace.join("src/fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let runtime = super::super::tests::in_process_runtime(
            &workspace,
            json!({
                "version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]
            }),
        );
        for (path, expected) in [
            ("src/missing", ToolDisposition::Failed),
            ("src/missing/child", ToolDisposition::Failed),
            ("src/file/child", ToolDisposition::Failed),
            ("src/file/child/deeper", ToolDisposition::Failed),
            ("src/link", ToolDisposition::Denied),
            ("src/link/child", ToolDisposition::Denied),
            ("src/directory-link/missing", ToolDisposition::Denied),
            ("src/.env", ToolDisposition::Denied),
            ("src/.env/child", ToolDisposition::Denied),
            ("src/fifo", ToolDisposition::Denied),
            ("src/fifo/child", ToolDisposition::Denied),
            ("src/hardlink", ToolDisposition::Denied),
            ("src/hardlink/child", ToolDisposition::Denied),
            ("outside/missing", ToolDisposition::Denied),
            ("../missing", ToolDisposition::Denied),
        ] {
            let intent = runtime.intent(
                crate::session::RunId::new(),
                crate::session::AgentRunId::new(),
                ToolCall::Read {
                    path: path.into(),
                    offset: None,
                    limit: None,
                },
            );
            let observation = runtime
                .execute(intent, super::super::Cancellation::new())
                .await;
            assert_eq!(observation.disposition, expected, "{path}");
            assert_eq!(
                observation.output,
                if expected == ToolDisposition::Failed {
                    "File or directory not found; List the parent directory to check the path"
                } else if observation.attestation.is_some() {
                    "File path is unavailable or outside the admitted regular-file boundary"
                } else {
                    "Tool operation is outside the pinned grant"
                },
                "{path}"
            );
            if expected == ToolDisposition::Failed {
                assert!(
                    matches!(observation.attestation, Some(Attestation::InProcess { .. })),
                    "{path}"
                );
            }
            assert_eq!(observation.valid(), observation.intent.valid(), "{path}");
        }
        assert!(open_absolute(workspace.join("src/file").to_str().unwrap()).is_ok());
        assert!(matches!(
            open_absolute(workspace.join("src/missing").to_str().unwrap()),
            Err(ToolError::Path)
        ));
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            ToolCall::Skill {
                name: "review".into(),
                resource: None,
            },
        );
        assert_eq!(
            super::super::dispatch::outcome(&intent, Err(ToolError::NotFound)),
            (
                ToolDisposition::Denied,
                "File path is unavailable or outside the admitted regular-file boundary".into(),
                None,
                None,
            )
        );
        #[cfg(target_os = "linux")]
        {
            let host_root = open_directory(Path::new("/")).unwrap();
            for path in ["dev", "dev/arany-missing-path"] {
                assert!(matches!(open_file(&host_root, path), Err(ToolError::Path)));
            }
        }
    }
}

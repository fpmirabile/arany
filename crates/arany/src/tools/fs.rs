use super::catalog::{nullable, string};
use super::config::Config;
use super::types::*;
use super::{ToolError, ToolRuntime};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::List { path } => valid_relative(path, true),
        ToolCall::Read { path, limit, .. } => {
            valid_relative(path, false) && (1..=MAX_READ_PAGE_BYTES).contains(limit)
        }
        ToolCall::Search { path, query } => {
            valid_relative(path, true) && !query.is_empty() && query.len() <= 512
        }
        ToolCall::Mkdir { path } => valid_relative(path, false),
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => {
            valid_relative(path, false)
                && content.len() <= 8 * 1024
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
                && old.len() + new.len() <= 8 * 1024
        }
        ToolCall::ApplyChangeset {
            source_intent_digest,
            changeset,
        } => *source_intent_digest != [0; 32] && !changeset.files.is_empty() && changeset.valid(),
        _ => false,
    }
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    let path = ("path", string());
    Some(match operation {
        "list" | "mkdir" => vec![path],
        "read" => vec![
            path,
            (
                "offset",
                json!({"type":"integer", "description":"Zero-based byte offset, 0 through 4294967295. Start at 0; page using the returned next offset."}),
            ),
            (
                "limit",
                json!({"type":"integer", "description":"Number of UTF-8 bytes to read: integer 1 through 65536 inclusive. Use 65536 for a normal page; never request the whole file with a larger limit."}),
            ),
        ],
        "search" => vec![path, ("query", string())],
        "write" => vec![path, ("expected_digest", nullable()), ("content", string())],
        "edit" => vec![
            path,
            (
                "expected_digest",
                json!({"type":"string", "description":"Exact lowercase 64-character SHA-256 from the file include or a successful Read. Never invent it."}),
            ),
            (
                "old",
                json!({"type":"string", "description":"Non-empty exact literal text occurring once in the current file. To append, select a unique ending and include that ending plus the appended text in new."}),
            ),
            (
                "new",
                json!({"type":"string", "description":"Replacement literal text. Combined old and new UTF-8 size must be at most 8192 bytes."}),
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
    root_identity: (u64, u64),
    entries: BTreeMap<String, Entry>,
}

pub(super) fn command_snapshot(workspace: &Path) -> Result<CommandSnapshot, ToolError> {
    let root = open_directory(workspace)?;
    check_change_directory(&root, &root)?;
    let mut entries = BTreeMap::new();
    let mut budget = ScanBudget::default();
    let mut retained = 0usize;
    command_collect(&root, "", &mut entries, &mut budget, &mut retained)?;
    Ok(CommandSnapshot {
        root_identity: identity(&root)?,
        entries,
    })
}

fn command_collect(
    root: &Dir,
    path: &str,
    entries: &mut BTreeMap<String, Entry>,
    budget: &mut ScanBudget,
    retained: &mut usize,
) -> Result<(), ToolError> {
    let directory = if path.is_empty() {
        root.try_clone().map_err(|_| ToolError::Path)?
    } else {
        Dir::from_std_file(open_file(root, path)?)
    };
    let mut names = Vec::new();
    for entry in directory.entries().map_err(|_| ToolError::Path)? {
        budget.entry()?;
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
        if !valid_relative(&selected, false) {
            return Err(if selected.split('/').any(forbidden) {
                ToolError::FilesystemDenied
            } else {
                ToolError::Path
            });
        }
        if selected.split('/').count() > 16 {
            return Err(ToolError::Limit);
        }
        names.push(selected);
    }
    names.sort();
    if names.is_empty() && !path.is_empty() {
        *retained += 1;
    }
    for selected in names {
        let file = open_file(root, &selected)?;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o6000 != 0 {
                return Err(ToolError::FilesystemDenied);
            }
        }
        let entry = if metadata.is_dir() {
            command_collect(root, &selected, entries, budget, retained)?;
            Entry {
                path: selected.clone(),
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
            let content = read_regular(file, MAX_FILE_BYTES)?;
            budget.bytes = budget
                .bytes
                .checked_add(content.len())
                .ok_or(ToolError::Limit)?;
            if budget.bytes > MAX_SNAPSHOT_BYTES {
                return Err(ToolError::Limit);
            }
            *retained += 1;
            Entry {
                path: selected.clone(),
                content: Some(content),
                executable,
            }
        };
        if *retained > MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        entries.insert(selected, entry);
    }
    Ok(())
}

pub(super) fn command_changeset(
    workspace: &Path,
    before: &CommandSnapshot,
) -> Result<CommandChangeset, ToolError> {
    let after = command_snapshot(workspace)?;
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
    open_file(
        &parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(ToolError::Path)?,
    )
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
        .map_err(|_| ToolError::Path)?;
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
                return Err(ToolError::Path);
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
                .map_err(|_| ToolError::Path)?,
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
    let dir = if parts.is_empty() {
        Dir::from_std_file(open_file(root, ".")?)
    } else {
        Dir::from_std_file(open_file(root, &parts.join("/"))?)
    };
    if !dir.dir_metadata().map_err(|_| ToolError::Path)?.is_dir() {
        return Err(ToolError::Path);
    }
    Ok((dir, leaf))
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

fn put(root: &Dir, path: &str, bytes: &[u8], executable: bool) -> Result<(), ToolError> {
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
                collect_root(workspace, "", &mut entries, &mut budget)?;
            } else {
                collect(workspace, path, &mut entries, &mut budget)?;
            }
        }
        for entry in entries {
            put_entry(target, entry)?;
        }
    }
    let mut runtime_bytes = 0usize;
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

fn collect(
    root: &Dir,
    path: &str,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
) -> Result<(), ToolError> {
    if !valid_relative(path, false) || path.split('/').count() > 16 {
        return Err(ToolError::Path);
    }
    let file = open_file(root, path)?;
    collect_open(root, path, file, output, budget)
}

fn collect_open(
    root: &Dir,
    path: &str,
    file: File,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
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
            if valid_relative(&name, false) {
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
            collect(root, &format!("{path}/{name}"), output, budget)?;
        }
    } else {
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

fn ignored_directory(name: &str) -> bool {
    matches!(name, "target" | "node_modules" | ".venv" | "venv")
}

fn collect_root(
    root: &Dir,
    path: &str,
    output: &mut Vec<Entry>,
    budget: &mut ScanBudget,
) -> Result<(), ToolError> {
    if !valid_relative(path, true) || path.split('/').count() > 16 {
        return Err(ToolError::Path);
    }
    let directory = if path.is_empty() {
        root.try_clone().map_err(|_| ToolError::Path)?
    } else {
        let file = open_file(root, path)?;
        if !file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
            return collect_open(root, path, file, output, budget);
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
        if !valid_relative(&name, false) || ignored_directory(&name) {
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
            collect_root(root, &selected, output, budget)?;
        } else {
            collect(root, &selected, output, budget)?;
        }
    }
    Ok(())
}

pub(crate) fn workspace_entries(
    workspace: &Path,
    folder: &str,
    prefix: &str,
) -> Result<Vec<String>, ToolError> {
    entries(&open_directory(workspace)?, folder, prefix)
}

fn entries(root: &Dir, folder: &str, prefix: &str) -> Result<Vec<String>, ToolError> {
    let folder = if folder == "." { "" } else { folder };
    if !valid_relative(folder, true)
        || folder.split('/').count() > 16
        || !valid_relative(prefix, true)
    {
        return Err(ToolError::Path);
    }
    if !prefix.is_empty() {
        let mut matches = Vec::new();
        find_entries(
            root,
            folder,
            &prefix.to_lowercase(),
            &mut ScanBudget::default(),
            &mut matches,
        )?;
        return Ok(matches.into_iter().map(|(_, name)| name).collect());
    }
    let directory = if folder.is_empty() {
        root.try_clone().map_err(|_| ToolError::Path)?
    } else {
        Dir::from_std_file(open_file(root, folder)?)
    };
    let mut names = Vec::new();
    for (index, entry) in directory
        .entries()
        .map_err(|_| ToolError::Path)?
        .enumerate()
    {
        if index >= MAX_SNAPSHOT_FILES * 4 {
            break;
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_relative(&name, false) || ignored_directory(&name) {
            continue;
        }
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|_| ToolError::Path)?;
        if metadata.is_symlink() || !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            if metadata.dev() != identity(root)?.0 || metadata.is_file() && metadata.nlink() != 1 {
                continue;
            }
        }
        let suffix = if metadata.is_dir() { "/" } else { "" };
        let relative = if folder.is_empty() {
            name
        } else {
            format!("{folder}/{name}")
        };
        if valid_relative(&relative, false) && relative.split('/').count() <= 16 {
            names.push(format!("{relative}{suffix}"));
        }
    }
    names.sort();
    names.truncate(64);
    Ok(names)
}

fn find_entries(
    root: &Dir,
    folder: &str,
    query: &str,
    budget: &mut ScanBudget,
    matches: &mut Vec<(u8, String)>,
) -> Result<(), ToolError> {
    let directory = if folder.is_empty() {
        root.try_clone().map_err(|_| ToolError::Path)?
    } else {
        Dir::from_std_file(open_file(root, folder)?)
    };
    let mut entries = Vec::new();
    for entry in directory.entries().map_err(|_| ToolError::Path)? {
        if budget.entry().is_err() {
            break;
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let relative = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
        if !valid_relative(&relative, false)
            || relative.split('/').count() > 16
            || ignored_directory(&name)
        {
            continue;
        }
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|_| ToolError::Path)?;
        if metadata.is_symlink() || !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            if metadata.dev() != identity(root)?.0 || metadata.is_file() && metadata.nlink() != 1 {
                continue;
            }
        }
        entries.push((relative, metadata.is_dir()));
    }
    entries.sort();
    for (relative, is_dir) in entries {
        if let Some(rank) = path_match(&relative, query) {
            matches.push((rank, format!("{relative}{}", if is_dir { "/" } else { "" })));
            matches.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then(left.1.len().cmp(&right.1.len()))
                    .then(left.1.cmp(&right.1))
            });
            matches.truncate(64);
        }
        if is_dir && budget.entries < MAX_SNAPSHOT_FILES * 4 {
            let _ = find_entries(root, &relative, query, budget, matches);
        }
    }
    Ok(())
}

fn path_match(path: &str, query: &str) -> Option<u8> {
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
        collect(&root, &name, &mut entries, &mut budget)?;
    }
    for entry in entries {
        put_entry(&target, entry)?;
    }
    Ok(())
}

/// Runs one in-process read under the admitted Workspace handle; output fits `result_bytes`
/// before it is serialized.
pub(super) fn inspect(
    root: &Dir,
    call: &ToolCall,
    config: &Config,
    result_bytes: usize,
) -> Result<String, ToolError> {
    match call {
        ToolCall::Read {
            path,
            offset,
            limit,
        } => {
            let bytes = read_regular(open_file(root, path)?, MAX_FILE_BYTES)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| ToolError::Operation)?;
            let start = *offset as usize;
            if start > text.len() || !text.is_char_boundary(start) {
                return Err(ToolError::Path);
            }
            let sha256 = hex_digest(&bytes);
            let page = |end: usize| {
                json!({"sha256":sha256,"offset":start,"next_offset":end,"truncated":end<bytes.len(),"text":&text[start..end]}).to_string()
            };
            // The longest envelope: the largest next offset, `false` and no text.
            let envelope = json!({"sha256":sha256,"offset":start,"next_offset":text.len(),"truncated":false,"text":""}).to_string().len();
            let budget = result_bytes.checked_sub(envelope).ok_or(ToolError::Limit)?;
            let mut end = start;
            let mut escaped = 0;
            for (index, character) in text[start..].char_indices() {
                let next = index + character.len_utf8();
                if next > *limit as usize {
                    break;
                }
                escaped += escaped_len(character);
                if escaped > budget {
                    if end == start {
                        return Err(ToolError::Limit);
                    }
                    break;
                }
                end = start + next;
            }
            let output = page(end);
            if output.len() > result_bytes {
                return Err(ToolError::Limit);
            }
            Ok(output)
        }
        ToolCall::List { path } | ToolCall::Search { path, .. } => {
            let path = if path == "." { "" } else { path.as_str() };
            if config.workspace_paths == ["."] && matches!(call, ToolCall::List { .. }) {
                let rows = entries(root, path, "")?;
                let truncated = rows.len() >= 64;
                return fit_rows(
                    rows.into_iter().map(Value::from).collect(),
                    truncated,
                    result_bytes,
                );
            }
            let mut files = Vec::new();
            let mut budget = ScanBudget::default();
            for selected in &config.workspace_paths {
                if selected == "." {
                    collect_root(root, path, &mut files, &mut budget)?;
                } else if path.is_empty() || Path::new(selected).starts_with(path) {
                    collect(root, selected, &mut files, &mut budget)?;
                } else if Path::new(path).starts_with(selected) {
                    collect(root, path, &mut files, &mut budget)?;
                }
            }
            let mut rows = Vec::new();
            for entry in files {
                let name = entry.path;
                let Some(content) = entry.content else {
                    if matches!(call, ToolCall::List { .. }) {
                        rows.push(json!({"path":name,"kind":"directory"}));
                        if rows.len() >= 64 {
                            break;
                        }
                    }
                    continue;
                };
                if let ToolCall::Search { query, .. } = call {
                    if let Ok(text) = std::str::from_utf8(&content) {
                        for (index, line) in text.lines().enumerate() {
                            if line.contains(query) {
                                rows.push(json!({"path":name,"line":index+1,"text":line.chars().take(512).collect::<String>()}));
                            }
                            if rows.len() == 64 {
                                break;
                            }
                        }
                    }
                } else {
                    rows.push(json!({"path":name,"bytes":content.len()}));
                }
                if rows.len() >= 64 {
                    break;
                }
            }
            let truncated = rows.len() >= 64;
            fit_rows(rows, truncated, result_bytes)
        }
        _ => Err(ToolError::Operation),
    }
}

/// Bytes `character` occupies inside a serialized JSON string.
fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        '\0'..='\u{1f}' => 6,
        _ => character.len_utf8(),
    }
}

/// Drops trailing rows until the listing fits `result_bytes`, marking it possibly truncated; a
/// listing with matches that keeps no row is a Limit, not an empty success.
fn fit_rows(
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

pub(super) fn native(
    workspace: &Path,
    call: &ToolCall,
    config: &Config,
) -> Result<String, ToolError> {
    let root = open_directory(workspace)?;
    match call {
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => replace(&root, path, expected_digest.as_deref(), content.as_bytes()),
        ToolCall::Mkdir { path } => {
            let (parent, leaf) = parent(&root, path)?;
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt;
                builder.mode(0o755);
            }
            parent.create_dir_with(&leaf, &builder).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ToolError::Conflict
                } else {
                    ToolError::Operation
                }
            })?;
            parent
                .into_std_file()
                .sync_all()
                .map_err(|_| ToolError::Uncertain)?;
            Ok(json!({"created":true}).to_string())
        }
        ToolCall::Edit {
            path,
            expected_digest,
            old,
            new,
        } => {
            let bytes = read_regular(open_file(&root, path)?, MAX_FILE_BYTES)?;
            if hex_digest(&bytes) != *expected_digest {
                return Err(ToolError::Conflict);
            }
            let text = String::from_utf8(bytes).map_err(|_| ToolError::Operation)?;
            if text.match_indices(old).count() != 1 {
                return Err(ToolError::Conflict);
            }
            let replacement = text.replacen(old, new, 1);
            if replacement.len() > MAX_FILE_BYTES {
                return Err(ToolError::Limit);
            }
            replace(&root, path, Some(expected_digest), replacement.as_bytes())
        }
        ToolCall::ApplyChangeset { changeset, .. } => {
            config.validate()?;
            if !config.allows(call) {
                return Err(ToolError::Path);
            }
            apply_changeset(&root, changeset)
        }
        _ => Err(ToolError::Operation),
    }
}

struct ChangeParent {
    directory: Dir,
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
                    directory,
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
        directory,
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

fn apply_changeset(root: &Dir, changeset: &CommandChangeset) -> Result<String, ToolError> {
    if !changeset.valid() {
        return Err(ToolError::Path);
    }
    check_change_directory(root, root)?;
    let mut parents = Vec::new();
    for change in &changeset.files {
        let parent = change_parent(root, &change.path)?;
        if parent.missing.is_empty() {
            check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())?;
        } else if change.before.is_some() {
            return Err(ToolError::Conflict);
        }
        parents.push(parent);
    }
    for (change, parent) in changeset.files.iter().zip(&parents) {
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
    for (change, mut parent) in changeset.files.iter().zip(parents) {
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
                    parent.directory = live;
                } else {
                    let mut builder = DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use cap_std::fs::DirBuilderExt;
                        builder.mode(0o700);
                    }
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
                    // Reopen "." readably first: a path-only capability handle cannot prove durability through fsync.
                    parent
                        .directory
                        .try_clone()
                        .map_err(|_| ToolError::Uncertain)?
                        .into_std_file()
                        .sync_all()
                        .map_err(|_| ToolError::Uncertain)?;
                    let live = Dir::from_std_file(open_file(&parent.directory, part)?);
                    check_change_directory(root, &live)?;
                    created.insert(path.clone(), identity(&live)?);
                    parent.directory = live;
                }
                parent.path = path;
            }
            verify_change_parent(root, &parent)?;
            let mode = check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())?;
            if let Some(after) = &change.after {
                let bytes = after.bytes().ok_or(ToolError::Path)?;
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
                    || {
                        verify_change_parent(root, &parent)?;
                        check_change_file(&parent.directory, &parent.leaf, change.before.as_ref())
                            .map(|_| ())
                    },
                )?;
            } else {
                parent
                    .directory
                    .remove_file(&parent.leaf)
                    .map_err(|_| ToolError::Operation)?;
                affected = true;
                parent
                    .directory
                    .try_clone()
                    .map_err(|_| ToolError::Uncertain)?
                    .into_std_file()
                    .sync_all()
                    .map_err(|_| ToolError::Uncertain)?;
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
    Ok(json!({"applied":changeset.files.len()}).to_string())
}

fn replace(
    root: &Dir,
    path: &str,
    expected: Option<&str>,
    bytes: &[u8],
) -> Result<String, ToolError> {
    let (parent, leaf) = parent(root, path)?;
    let verify = || match open_file(&parent, &leaf) {
        Ok(file) => {
            if expected.is_some_and(|hash| {
                read_regular(file, MAX_FILE_BYTES).is_ok_and(|current| hex_digest(&current) == hash)
            }) {
                Ok(())
            } else {
                Err(ToolError::Conflict)
            }
        }
        Err(_)
            if expected.is_none()
                && parent
                    .symlink_metadata(&leaf)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(())
        }
        Err(_) => Err(ToolError::Conflict),
    };
    verify()?;
    #[cfg(unix)]
    let mode = match parent.symlink_metadata(&leaf) {
        Ok(metadata) => {
            use cap_std::fs::MetadataExt;
            metadata.mode() & 0o777
        }
        Err(_) => 0o600,
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    replace_file(&parent, &leaf, expected.is_none(), bytes, mode, verify)?;
    Ok(json!({"sha256":hex_digest(bytes),"bytes":bytes.len()}).to_string())
}

fn replace_file(
    parent: &Dir,
    leaf: &str,
    create: bool,
    bytes: &[u8],
    mode: u32,
    verify: impl Fn() -> Result<(), ToolError>,
) -> Result<(), ToolError> {
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
            parent
                .try_clone()
                .map_err(|_| ToolError::Uncertain)?
                .into_std_file()
                .sync_all()
                .map_err(|_| ToolError::Uncertain)?;
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
    if identity(&open_directory(&runtime.workspace)?)? != runtime.workspace_identity {
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

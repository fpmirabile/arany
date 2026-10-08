use super::config::Config;
use super::types::*;
use super::{ToolError, ToolRuntime};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(super) struct Snapshot {
    pub path: PathBuf,
    pub dir: Dir,
}

struct Entry {
    path: String,
    content: Option<Vec<u8>>,
    executable: bool,
}

pub(crate) fn open_directory(path: &Path) -> Result<Dir, ToolError> {
    if !path.is_absolute() {
        return Err(ToolError::Path);
    }
    let mut dir =
        Dir::open_ambient_dir("/", cap_std::ambient_authority()).map_err(|_| ToolError::Path)?;
    for component in path.components().skip(1) {
        let std::path::Component::Normal(name) = component else {
            return Err(ToolError::Path);
        };
        dir = dir.open_dir_nofollow(name).map_err(|_| ToolError::Path)?;
    }
    Ok(dir)
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
    let parent = open_directory(path.parent().ok_or(ToolError::Path)?)?;
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
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
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
                    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
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
    for part in parts {
        if let Err(error) = dir.create_dir(part)
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
        match parent.create_dir(&leaf) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => parent
                .open_dir_nofollow(&leaf)
                .map(|_| ())
                .map_err(|_| ToolError::Path),
            Err(_) => Err(ToolError::Operation),
        }
    }
}

impl Snapshot {
    pub(super) fn create(
        workspace: &Dir,
        config: &Config,
        include_workspace: bool,
    ) -> Result<Self, ToolError> {
        let tmp = open_directory(snapshot_root())?;
        let name = format!("arany-tools-{}", uuid::Uuid::now_v7());
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        tmp.create_dir_with(&name, &builder)
            .map_err(|_| ToolError::Operation)?;
        let dir = tmp.open_dir_nofollow(&name).map_err(|_| ToolError::Path)?;
        let snapshot = Self {
            path: snapshot_root().join(&name),
            dir,
        };
        snapshot
            .dir
            .create_dir("workspace")
            .map_err(|_| ToolError::Operation)?;
        let target = snapshot
            .dir
            .open_dir_nofollow("workspace")
            .map_err(|_| ToolError::Path)?;
        let mut entries = Vec::new();
        let mut budget = ScanBudget::default();
        if include_workspace {
            for path in &config.workspace_paths {
                if path == "." {
                    collect_root(workspace, "", &mut entries, &mut budget)?;
                } else {
                    collect(workspace, path, &mut entries, &mut budget)?;
                }
            }
        }
        for entry in entries {
            put_entry(&target, entry)?;
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
            put(
                &snapshot.dir,
                &format!("programs/{}", program.name),
                &executable,
                true,
            )?;
            for input in &program.inputs {
                let content = read_regular(open_absolute(&input.path)?, MAX_FILE_BYTES)?;
                if hex_digest(&content) != input.sha256 {
                    return Err(ToolError::ChangedInput);
                }
                charge_runtime(&mut runtime_bytes, content.len())?;
                put(
                    &snapshot.dir,
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
                    &snapshot.dir,
                    &format!("skills/{}/{path}", skill.name),
                    &content,
                    false,
                )?;
            }
        }
        Ok(snapshot)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if let (Ok(tmp), Some(name)) = (open_directory(snapshot_root()), self.path.file_name()) {
            let _ = tmp.remove_dir_all(name);
        }
    }
}

fn snapshot_root() -> &'static Path {
    #[cfg(target_os = "macos")]
    {
        Path::new("/private/tmp")
    }
    #[cfg(not(target_os = "macos"))]
    {
        Path::new("/tmp")
    }
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
    let folder = if folder == "." { "" } else { folder };
    if !valid_relative(folder, true)
        || folder.split('/').count() > 16
        || !valid_relative(prefix, true)
    {
        return Err(ToolError::Path);
    }
    let root = open_directory(workspace)?;
    if !prefix.is_empty() {
        let mut matches = Vec::new();
        find_entries(
            &root,
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
        Dir::from_std_file(open_file(&root, folder)?)
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
            if metadata.dev() != identity(&root)?.0 || metadata.is_file() && metadata.nlink() != 1 {
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

pub(super) fn native(call: &ToolCall, config: &Config) -> Result<String, ToolError> {
    let root = open_directory(Path::new("/workspace"))?;
    match call {
        ToolCall::Read {
            path,
            offset,
            limit,
        } => {
            let bytes = read_regular(open_file(&root, path)?, MAX_FILE_BYTES)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| ToolError::Operation)?;
            let start = *offset as usize;
            if start > text.len() || !text.is_char_boundary(start) {
                return Err(ToolError::Path);
            }
            let mut end = start.saturating_add(*limit as usize).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            Ok(json!({"sha256":hex_digest(&bytes),"offset":start,"next_offset":end,"truncated":end<bytes.len(),"text":&text[start..end]}).to_string())
        }
        ToolCall::List { path } | ToolCall::Search { path, .. } => {
            let path = if path == "." { "" } else { path.as_str() };
            if config.workspace_paths == ["."] && matches!(call, ToolCall::List { .. }) {
                let rows = workspace_entries(Path::new("/workspace"), path, "")?;
                return Ok(
                    json!({"entries":rows,"limit":64,"may_be_truncated":rows.len()>=64})
                        .to_string(),
                );
            }
            let mut files = Vec::new();
            let mut budget = ScanBudget::default();
            for selected in &config.workspace_paths {
                if selected == "." {
                    collect_root(&root, path, &mut files, &mut budget)?;
                } else if path.is_empty() || Path::new(selected).starts_with(path) {
                    collect(&root, selected, &mut files, &mut budget)?;
                } else if Path::new(path).starts_with(selected) {
                    collect(&root, path, &mut files, &mut budget)?;
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
            Ok(json!({"entries":rows,"limit":64,"may_be_truncated":rows.len()>=64}).to_string())
        }
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
        _ => Err(ToolError::Operation),
    }
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
        #[cfg(target_os = "linux")]
        {
            if expected.is_none() {
                rustix::fs::renameat_with(
                    &parent,
                    &temporary,
                    &parent,
                    &leaf,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(|_| ToolError::Conflict)?;
            } else {
                parent
                    .rename(&temporary, &parent, &leaf)
                    .map_err(|_| ToolError::Operation)?;
            }
            parent
                .try_clone()
                .map_err(|_| ToolError::Uncertain)?
                .into_std_file()
                .sync_all()
                .map_err(|_| ToolError::Uncertain)?;
            Ok(json!({"sha256":hex_digest(bytes),"bytes":bytes.len()}).to_string())
        }
        #[cfg(not(target_os = "linux"))]
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

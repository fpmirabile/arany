use super::*;
use crate::tools::selection::{CONTROL_BYTES, Selector};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[cfg(target_os = "linux")]
pub(in crate::tools) mod overlay;

const SELECTION_FILE: &str = "command-selection.json";
const SELECTION_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools) struct CommandSelection {
    controls: BTreeMap<String, String>,
    omitted: BTreeSet<String>,
    pruned: BTreeSet<String>,
    pub(in crate::tools) summary: CommandSelectionSummary,
    calls: u64,
}

impl CommandSelection {
    #[cfg(target_os = "linux")]
    pub(in crate::tools) fn receive(reader: &mut impl Read) -> Result<Self, ToolError> {
        let mut length = [0; 8];
        reader
            .read_exact(&mut length)
            .map_err(|_| ToolError::Operation)?;
        let length = u64::from_le_bytes(length);
        if length > SELECTION_BYTES as u64 {
            return Err(ToolError::Limit);
        }
        let mut bytes = vec![0; length as usize];
        reader
            .read_exact(&mut bytes)
            .map_err(|_| ToolError::Operation)?;
        Self::decode(&bytes)
    }

    #[cfg(target_os = "linux")]
    pub(in crate::tools) async fn send(
        resources: &Dir,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<(), ToolError> {
        use tokio::io::AsyncWriteExt;
        let mut file = open_file(resources, SELECTION_FILE)?;
        let metadata = safe_metadata(&file)?;
        if metadata.len() > SELECTION_BYTES as u64 {
            return Err(ToolError::Limit);
        }
        writer
            .write_all(&metadata.len().to_le_bytes())
            .await
            .map_err(|_| ToolError::Operation)?;
        let mut buffer = [0; 16 * 1024];
        let mut left = metadata.len();
        while left > 0 {
            let length = buffer.len().min(left as usize);
            file.read_exact(&mut buffer[..length])
                .map_err(|_| ToolError::Operation)?;
            writer
                .write_all(&buffer[..length])
                .await
                .map_err(|_| ToolError::Operation)?;
            left -= length as u64;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(in crate::tools) fn read(resources: &Path) -> Result<Self, ToolError> {
        let root = open_directory(resources)?;
        let bytes = read_regular(open_file(&root, SELECTION_FILE)?, SELECTION_BYTES)?;
        Self::decode(&bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ToolError> {
        if bytes.len() > SELECTION_BYTES {
            return Err(ToolError::Limit);
        }
        let selection: Self = serde_json::from_slice(bytes).map_err(|_| ToolError::Path)?;
        if selection.omitted.len() + selection.pruned.len() > COMMAND_SCAN_ENTRIES
            || selection.calls > crate::tools::selection::COMMAND_CALLS
            || selection.controls.len() > crate::tools::selection::CONTROL_FILES
            || selection
                .controls
                .values()
                .any(|bytes| bytes.len() > CONTROL_BYTES)
            || selection.controls.values().map(String::len).sum::<usize>()
                > crate::tools::selection::CONTROL_TOTAL_BYTES
            || selection
                .summary
                .ignored_entries
                .checked_add(selection.summary.oversized_inputs)
                .is_none_or(|count| count > COMMAND_SCAN_ENTRIES as u64)
            || selection.summary.discarded_outputs > COMMAND_SCAN_ENTRIES as u64
        {
            return Err(ToolError::Limit);
        }
        if !selection.summary.valid()
            || selection
                .controls
                .keys()
                .any(|path| !path.is_empty() && !valid_relative(path, false))
            || selection
                .omitted
                .iter()
                .chain(&selection.pruned)
                .any(|path| !valid_relative(path, false))
        {
            return Err(ToolError::Path);
        }
        if selection
            .controls
            .keys()
            .any(|path| !path.is_empty() && path.split('/').count() + 1 > COMMAND_SNAPSHOT_DEPTH)
            || selection
                .omitted
                .iter()
                .chain(&selection.pruned)
                .any(|path| path.split('/').count() > COMMAND_SNAPSHOT_DEPTH)
        {
            return Err(ToolError::Limit);
        }
        Ok(selection)
    }

    // Outputs beneath a pruned directory (ignored or a nested repository) are never published.
    fn prunes(&self, path: &str) -> bool {
        path.match_indices('/')
            .any(|(index, _)| self.pruned.contains(&path[..index]))
    }

    fn selector(&self, deadline: Instant) -> Result<Selector, ToolError> {
        let mut selector = Selector::new(self.calls, deadline);
        for (directory, bytes) in &self.controls {
            selector.add(directory, bytes.as_bytes())?;
        }
        Ok(selector)
    }
}

#[cfg(any(test, target_os = "macos"))]
struct Inputs<'a> {
    source: &'a Dir,
    target: &'a Dir,
    config: &'a Config,
    selection: CommandSelection,
    selector: Selector,
    budget: CommandScanBudget,
    retained: usize,
    controls: BTreeMap<String, Option<(std::fs::Metadata, Vec<u8>)>>,
    device: u64,
}

#[cfg(any(test, target_os = "macos"))]
pub(in crate::tools) fn prepare_command(
    source: &Dir,
    target: &Dir,
    resources: &Dir,
    config: &Config,
    deadline: Instant,
) -> Result<CommandSelection, ToolError> {
    select_command(source, target, resources, config, deadline).map_err(ToolError::in_selection)
}

#[cfg(any(test, target_os = "macos"))]
fn select_command(
    source: &Dir,
    target: &Dir,
    resources: &Dir,
    config: &Config,
    deadline: Instant,
) -> Result<CommandSelection, ToolError> {
    config.validate()?;
    let started = Instant::now();
    let mut input = Inputs {
        source,
        target,
        config,
        selection: CommandSelection::default(),
        selector: Selector::new(0, deadline),
        budget: CommandScanBudget::default(),
        retained: 0,
        controls: BTreeMap::new(),
        device: identity(source)?.0,
    };
    input.control("", source)?;
    for path in &config.workspace_paths {
        if path == "." {
            input.directory("", source.try_clone().map_err(|_| ToolError::Path)?)?;
        } else {
            for (index, _) in path.match_indices('/') {
                let ancestor = &path[..index];
                if input.selector.excluded(ancestor, true)? {
                    return Err(ToolError::FilesystemDenied);
                }
                input.control(ancestor, &Dir::from_std_file(open_file(source, ancestor)?))?;
            }
            input.entry(path)?;
        }
    }
    for (path, control) in &input.controls {
        input.selector.check_time()?;
        let Some((metadata, bytes)) = control else {
            if !matches!(open_file(source, path), Err(ToolError::NotFound)) {
                return Err(ToolError::ChangedInput);
            }
            continue;
        };
        let current = open_file(source, path)?;
        if !same_file(metadata, &current.metadata().map_err(|_| ToolError::Path)?)
            || read_regular(current, CONTROL_BYTES)? != *bytes
        {
            return Err(ToolError::ChangedInput);
        }
    }
    input.selection.summary.elapsed_ms =
        Some(u64::try_from(started.elapsed().as_millis()).map_err(|_| ToolError::Limit)?);
    input.selection.calls = input.selector.calls();
    let bytes = serde_json::to_vec(&input.selection).map_err(|_| ToolError::Operation)?;
    if bytes.len() > SELECTION_BYTES {
        return Err(ToolError::Limit);
    }
    put(resources, SELECTION_FILE, &bytes, false)?;
    input.selector.check_time()?;
    Ok(input.selection)
}

#[cfg(any(test, target_os = "macos"))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            a.dev(),
            a.ino(),
            a.len(),
            a.mtime(),
            a.mtime_nsec(),
            a.ctime(),
            a.ctime_nsec(),
        ) == (
            b.dev(),
            b.ino(),
            b.len(),
            b.mtime(),
            b.mtime_nsec(),
            b.ctime(),
            b.ctime_nsec(),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

fn command_metadata(file: &File) -> Result<std::fs::Metadata, ToolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata().map_err(|_| ToolError::Path)?.mode() & 0o6000 != 0 {
            return Err(ToolError::FilesystemDenied);
        }
    }
    safe_metadata(file)
}

fn safe_metadata(file: &File) -> Result<std::fs::Metadata, ToolError> {
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(ToolError::Path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o6000 != 0 || metadata.is_file() && metadata.nlink() != 1 {
            return Err(ToolError::Path);
        }
    }
    Ok(metadata)
}

#[cfg(any(test, target_os = "macos"))]
impl Inputs<'_> {
    fn control(&mut self, directory: &str, handle: &Dir) -> Result<(), ToolError> {
        let path = if directory.is_empty() {
            ".gitignore".into()
        } else {
            format!("{directory}/.gitignore")
        };
        if self.controls.contains_key(&path) {
            return Ok(());
        }
        let file = match open_child(handle, ".gitignore", self.device) {
            Ok(file) => file,
            Err(ToolError::NotFound) => {
                self.controls.insert(path, None);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
            return Err(ToolError::Limit);
        }
        if !self
            .config
            .workspace_paths
            .iter()
            .any(|root| root == "." || Path::new(&path).starts_with(root))
        {
            return Err(ToolError::FilesystemDenied);
        }
        let metadata = safe_metadata(&file)?;
        let bytes = read_regular(file, CONTROL_BYTES)?;
        self.selector.add(directory, &bytes)?;
        self.selection.controls.insert(
            directory.into(),
            String::from_utf8(bytes.clone()).map_err(|_| ToolError::Path)?,
        );
        self.controls.insert(path, Some((metadata, bytes)));
        Ok(())
    }

    fn directory(&mut self, path: &str, directory: Dir) -> Result<(), ToolError> {
        self.control(path, &directory)?;
        let mut names = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            self.selector.check_time()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let valid_name = valid_relative(&name, false);
            let selected = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            self.budget.entry(&selected)?;
            if valid_name {
                names.push(selected);
            }
        }
        names.sort();
        if !path.is_empty() {
            put_entry(
                self.target,
                Entry {
                    path: path.into(),
                    content: None,
                    executable: false,
                },
            )?;
        }
        if names.is_empty() && !path.is_empty() {
            self.retain()?;
        }
        for name in names {
            self.entry_scanned(&name, &directory)?;
        }
        Ok(())
    }

    fn retain(&mut self) -> Result<(), ToolError> {
        self.retained += 1;
        if self.retained > COMMAND_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    fn entry(&mut self, path: &str) -> Result<(), ToolError> {
        self.budget.entry(path)?;
        let parent = path.rsplit_once('/').map_or(".", |(parent, _)| parent);
        let parent = Dir::from_std_file(open_file(self.source, parent)?);
        self.entry_scanned(path, &parent)
    }

    fn entry_scanned(&mut self, path: &str, parent: &Dir) -> Result<(), ToolError> {
        self.selector.check_time()?;
        if !valid_relative(path, false) || path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
            return Err(ToolError::Limit);
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let link = parent.symlink_metadata(name).map_err(|_| ToolError::Path)?;
        if path.split('/').any(ignored_directory) || self.selector.excluded(path, link.is_dir())? {
            self.selection.summary.ignored_entries += 1;
            self.selection.summary.pruned_directories += u64::from(link.is_dir());
            if link.is_dir() {
                self.selection.pruned.insert(path.into());
            }
            return Ok(());
        }
        if !link.is_dir() && !link.is_file() {
            return Err(ToolError::Path);
        }
        let file = open_child(parent, name, self.device)?;
        let metadata = command_metadata(&file)?;
        if metadata.is_dir() != link.is_dir() {
            return Err(ToolError::ChangedInput);
        }
        if metadata.is_dir() {
            let directory = Dir::from_std_file(file);
            if !self.config.workspace_paths.iter().any(|root| root == path)
                && nested_repository(&directory)?
            {
                self.selection.summary.ignored_entries += 1;
                self.selection.summary.pruned_directories += 1;
                self.selection.pruned.insert(path.into());
                return Ok(());
            }
            return self.directory(path, directory);
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            self.selection.omitted.insert(path.into());
            self.selection.summary.oversized_inputs += 1;
            return Ok(());
        }
        let content = read_regular(file, MAX_FILE_BYTES)?;
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
                path: path.into(),
                content: Some(content),
                executable,
            },
        )
    }
}

pub(in crate::tools) fn selected_command_changeset(
    workspace: &Path,
    before: &CommandSnapshot,
    selection: &CommandSelection,
    deadline: Instant,
) -> Result<(CommandChangeset, CommandSelectionSummary), ToolError> {
    let root = open_directory(workspace)?;
    if workspace_identity(&root)? != before.root_identity {
        return Err(ToolError::ChangedInput);
    }
    let mut capture = CommandCapture {
        device: identity(&root)?.0,
        root: &root,
        before,
        selection,
        selector: selection.selector(deadline)?,
        budget: CommandScanBudget::default(),
        summary: selection.summary,
        tree: OutputTree::default(),
        retained: 0,
        changes: ChangesetCapture::default(),
    };
    capture.scan()?;
    capture.finish()
}

#[derive(Default)]
struct OutputTree {
    directories: BTreeSet<String>,
    files: BTreeMap<String, std::fs::Metadata>,
}

struct CommandCapture<'a> {
    root: &'a Dir,
    device: u64,
    before: &'a CommandSnapshot,
    selection: &'a CommandSelection,
    selector: Selector,
    budget: CommandScanBudget,
    summary: CommandSelectionSummary,
    tree: OutputTree,
    retained: usize,
    changes: ChangesetCapture,
}

#[derive(Default)]
struct ChangesetCapture {
    files: Vec<FileChange>,
    bytes: usize,
}

impl ChangesetCapture {
    fn push(
        &mut self,
        path: &str,
        old: Option<&Entry>,
        after: Option<FileVersion>,
        bytes: usize,
    ) -> Result<(), ToolError> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(ToolError::Limit)?;
        if self.bytes > MAX_CHANGESET_BYTES || self.files.len() >= MAX_CHANGESET_FILES {
            return Err(ToolError::Limit);
        }
        let before = old.and_then(|entry| {
            entry
                .content
                .as_ref()
                .map(|content| FileVersion::from_bytes(content, entry.executable))
        });
        self.files.push(FileChange {
            path: path.into(),
            before,
            after,
        });
        Ok(())
    }
}

impl CommandCapture<'_> {
    fn scan(&mut self) -> Result<(), ToolError> {
        let mut pending = vec![String::new()];
        while let Some(path) = pending.pop() {
            self.selector.check_time()?;
            let directory = if path.is_empty() {
                self.root.try_clone().map_err(|_| ToolError::Path)?
            } else {
                Dir::from_std_file(open_file(self.root, &path)?)
            };
            let mut children = self.children(&directory, &path)?;
            if children.is_empty() && !path.is_empty() {
                self.retain()?;
            }
            children.reverse();
            for child in children {
                self.scan_entry(&directory, child, &mut pending)?;
            }
        }
        Ok(())
    }

    fn children(&mut self, directory: &Dir, parent: &str) -> Result<Vec<String>, ToolError> {
        let mut children = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            self.selector.check_time()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let path = if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            };
            self.budget.entry(&path)?;
            children.push(path);
        }
        children.sort();
        Ok(children)
    }

    fn scan_entry(
        &mut self,
        parent: &Dir,
        path: String,
        pending: &mut Vec<String>,
    ) -> Result<(), ToolError> {
        if !valid_relative(&path, false) {
            return Err(if path.split('/').any(forbidden) {
                ToolError::FilesystemDenied
            } else {
                ToolError::Path
            });
        }
        if path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
            return Err(ToolError::Limit);
        }
        if self.selection.omitted.contains(&path) {
            return Err(ToolError::Conflict);
        }
        let name = path.rsplit('/').next().unwrap_or(&path);
        let file = open_child(parent, name, self.device)?;
        let metadata = command_metadata(&file)?;
        self.validate_type(&path, metadata.is_dir())?;
        if metadata.is_dir() {
            self.tree.directories.insert(path.clone());
            pending.push(path);
        } else {
            self.capture_file(&path, &metadata, file)?;
            self.tree.files.insert(path, metadata);
        }
        Ok(())
    }

    fn validate_type(&self, path: &str, directory: bool) -> Result<(), ToolError> {
        let Some(before) = self.before.entries.get(path) else {
            return Ok(());
        };
        if before.content.is_none() != directory {
            return Err(ToolError::Path);
        }
        Ok(())
    }

    fn retain(&mut self) -> Result<(), ToolError> {
        self.retained += 1;
        if self.retained > COMMAND_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(CommandChangeset, CommandSelectionSummary), ToolError> {
        self.capture_deletions()?;
        self.changes
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        let changeset = CommandChangeset {
            files: self.changes.files,
        };
        if !changeset.valid() {
            return Err(ToolError::Limit);
        }
        self.selector.check_time()?;
        Ok((changeset, self.summary))
    }

    fn capture_file(
        &mut self,
        path: &str,
        metadata: &std::fs::Metadata,
        file: File,
    ) -> Result<(), ToolError> {
        self.selector.check_time()?;
        let old = self.before.entries.get(path);
        if old.is_none()
            && (path.split('/').any(ignored_directory)
                || self.selection.prunes(path)
                || self.selector.excluded(path, false)?)
        {
            self.summary.discarded_outputs += 1;
            return Ok(());
        }
        self.retain()?;
        let remaining = COMMAND_SNAPSHOT_BYTES.saturating_sub(self.budget_bytes());
        let content = read_regular(file, MAX_FILE_BYTES.min(remaining))?;
        self.budget.content(content.len())?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        if old.is_some_and(|entry| {
            entry.content.as_deref() == Some(content.as_slice()) && entry.executable == executable
        }) {
            return Ok(());
        }
        let old_bytes = old
            .and_then(|entry| entry.content.as_ref())
            .map_or(0, Vec::len);
        self.changes.push(
            path,
            old,
            Some(FileVersion::from_bytes(&content, executable)),
            old_bytes + content.len(),
        )
    }

    fn capture_deletions(&mut self) -> Result<(), ToolError> {
        for (path, entry) in &self.before.entries {
            let Some(content) = &entry.content else {
                continue;
            };
            if self.tree.files.contains_key(path) || self.tree.directories.contains(path) {
                continue;
            }
            self.changes.push(path, Some(entry), None, content.len())?;
        }
        Ok(())
    }

    fn budget_bytes(&self) -> usize {
        self.budget.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_selection_distinguishes_limits_from_invalid_data() {
        for (field, replacement, limit) in [
            (
                "calls",
                serde_json::json!(crate::tools::selection::COMMAND_CALLS + 1),
                true,
            ),
            (
                "summary",
                serde_json::json!({"elapsed_ms":u64::from(MAX_COMMAND_TIMEOUT_MS) + 1,
                    "ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0}),
                false,
            ),
            ("omitted", serde_json::json!(["../outside"]), false),
            ("omitted", serde_json::json!([""]), false),
            (
                "omitted",
                serde_json::json!([vec!["deep"; COMMAND_SNAPSHOT_DEPTH + 1].join("/")]),
                true,
            ),
            ("controls", serde_json::json!({"../outside":"x"}), false),
            (
                "controls",
                serde_json::json!({vec!["deep"; COMMAND_SNAPSHOT_DEPTH].join("/"):"x"}),
                true,
            ),
            (
                "controls",
                serde_json::json!({"":"x".repeat(CONTROL_BYTES + 1)}),
                true,
            ),
            (
                "summary",
                serde_json::json!({"elapsed_ms":null,"ignored_entries":0,"pruned_directories":1,"oversized_inputs":0,"discarded_outputs":0}),
                false,
            ),
        ] {
            let mut value = serde_json::to_value(CommandSelection::default()).unwrap();
            value[field] = replacement;
            let bytes = serde_json::to_vec(&value).unwrap();
            let error = CommandSelection::decode(&bytes).err().unwrap();
            assert!(
                if limit {
                    matches!(error, ToolError::Limit)
                } else {
                    matches!(error, ToolError::Path)
                },
                "{field}"
            );
            #[cfg(target_os = "linux")]
            {
                let mut frame = (bytes.len() as u64).to_le_bytes().to_vec();
                frame.extend_from_slice(&bytes);
                let error = CommandSelection::receive(&mut frame.as_slice())
                    .err()
                    .unwrap();
                assert!(
                    if limit {
                        matches!(error, ToolError::Limit)
                    } else {
                        matches!(error, ToolError::Path)
                    },
                    "framed {field}"
                );
            }
        }
    }
}

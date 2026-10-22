use super::*;
use crate::tools::selection::{CONTROL_BYTES, Selector};
use serde::{Deserialize, Serialize};
use std::time::Instant;

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
        if selection.omitted.len() + selection.pruned.len() > MAX_SNAPSHOT_FILES * 4
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
                .is_none_or(|count| count > (MAX_SNAPSHOT_FILES * 4) as u64)
            || selection.summary.discarded_outputs > (MAX_SNAPSHOT_FILES * 4) as u64
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
            .any(|path| !path.is_empty() && path.split('/').count() + 1 > 16)
            || selection
                .omitted
                .iter()
                .chain(&selection.pruned)
                .any(|path| path.split('/').count() > 16)
        {
            return Err(ToolError::Limit);
        }
        Ok(selection)
    }

    fn selector(&self, deadline: Instant) -> Result<Selector, ToolError> {
        let mut selector = Selector::new(self.calls, deadline);
        for (directory, bytes) in &self.controls {
            selector.add(directory, bytes.as_bytes())?;
        }
        Ok(selector)
    }
}

struct Inputs<'a> {
    source: &'a Dir,
    target: &'a Dir,
    config: &'a Config,
    selection: CommandSelection,
    selector: Selector,
    budget: ScanBudget,
    retained: usize,
    controls: BTreeMap<String, Option<(std::fs::Metadata, Vec<u8>)>>,
}

pub(in crate::tools) fn prepare_command(
    source: &Dir,
    target: &Dir,
    resources: &Dir,
    config: &Config,
    deadline: Instant,
) -> Result<CommandSelection, ToolError> {
    config.validate()?;
    let mut input = Inputs {
        source,
        target,
        config,
        selection: CommandSelection::default(),
        selector: Selector::new(0, deadline),
        budget: ScanBudget::default(),
        retained: 0,
        controls: BTreeMap::new(),
    };
    input.control("")?;
    for path in &config.workspace_paths {
        if path == "." {
            input.directory("")?;
        } else {
            for (index, _) in path.match_indices('/') {
                let ancestor = &path[..index];
                if input.selector.excluded(ancestor, true)? {
                    return Err(ToolError::FilesystemDenied);
                }
                input.control(ancestor)?;
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
    input.selection.calls = input.selector.calls();
    let bytes = serde_json::to_vec(&input.selection).map_err(|_| ToolError::Operation)?;
    if bytes.len() > SELECTION_BYTES {
        return Err(ToolError::Limit);
    }
    put(resources, SELECTION_FILE, &bytes, false)?;
    input.selector.check_time()?;
    Ok(input.selection)
}

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

impl Inputs<'_> {
    fn control(&mut self, directory: &str) -> Result<(), ToolError> {
        let path = if directory.is_empty() {
            ".gitignore".into()
        } else {
            format!("{directory}/.gitignore")
        };
        if self.controls.contains_key(&path) {
            return Ok(());
        }
        let file = match open_file(self.source, &path) {
            Ok(file) => file,
            Err(ToolError::NotFound) => {
                self.controls.insert(path, None);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if path.split('/').count() > 16 {
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

    fn directory(&mut self, path: &str) -> Result<(), ToolError> {
        self.control(path)?;
        let directory = if path.is_empty() {
            self.source.try_clone().map_err(|_| ToolError::Path)?
        } else {
            Dir::from_std_file(open_file(self.source, path)?)
        };
        let mut names = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            self.selector.check_time()?;
            self.budget.entry()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            if valid_relative(&name, false) {
                names.push(if path.is_empty() {
                    name
                } else {
                    format!("{path}/{name}")
                });
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
            self.entry(&name)?;
        }
        Ok(())
    }

    fn retain(&mut self) -> Result<(), ToolError> {
        self.retained += 1;
        if self.retained > MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    fn entry(&mut self, path: &str) -> Result<(), ToolError> {
        self.selector.check_time()?;
        if !valid_relative(path, false) || path.split('/').count() > 16 {
            return Err(ToolError::Limit);
        }
        let file = open_file(self.source, path)?;
        let metadata = safe_metadata(&file)?;
        if path.split('/').any(ignored_directory)
            || self.selector.excluded(path, metadata.is_dir())?
        {
            self.selection.summary.ignored_entries += 1;
            self.selection.summary.pruned_directories += u64::from(metadata.is_dir());
            if metadata.is_dir() {
                self.selection.pruned.insert(path.into());
            }
            return Ok(());
        }
        if metadata.is_dir() {
            return self.directory(path);
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            self.selection.omitted.insert(path.into());
            self.selection.summary.oversized_inputs += 1;
            return Ok(());
        }
        let content = read_regular(file, MAX_FILE_BYTES)?;
        self.budget.bytes = self
            .budget
            .bytes
            .checked_add(content.len())
            .ok_or(ToolError::Limit)?;
        if self.budget.bytes > MAX_SNAPSHOT_BYTES {
            return Err(ToolError::Limit);
        }
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
    let mut after = CommandSnapshot {
        root_identity: workspace_identity(&root)?,
        entries: BTreeMap::new(),
    };
    let mut selector = selection.selector(deadline)?;
    let mut budget = ScanBudget::default();
    let mut summary = selection.summary;
    let mut pending = vec![String::new()];
    let mut files = Vec::new();
    let mut retained = 0;
    while let Some(path) = pending.pop() {
        selector.check_time()?;
        let directory = if path.is_empty() {
            root.try_clone().map_err(|_| ToolError::Path)?
        } else {
            Dir::from_std_file(open_file(&root, &path)?)
        };
        let mut empty = true;
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            selector.check_time()?;
            empty = false;
            budget.entry()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let path = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            if !valid_relative(&path, false) {
                return Err(ToolError::Path);
            }
            if path.split('/').count() > 16 {
                return Err(ToolError::Limit);
            }
            let file = open_file(&root, &path)?;
            let metadata = safe_metadata(&file)?;
            if selection.omitted.contains(&path) {
                return Err(ToolError::Conflict);
            }
            if metadata.is_dir() {
                pending.push(path.clone());
                after.entries.insert(
                    path.clone(),
                    Entry {
                        path,
                        content: None,
                        executable: false,
                    },
                );
            } else {
                files.push((path, metadata));
            }
        }
        if empty && !path.is_empty() {
            retained += 1;
            if retained > MAX_SNAPSHOT_FILES {
                return Err(ToolError::Limit);
            }
        }
    }
    // Validate the complete tree before reading any publishable content.
    for (path, metadata) in files {
        selector.check_time()?;
        if !before.entries.contains_key(&path)
            && (path.split('/').any(ignored_directory) || selector.excluded(&path, false)?)
        {
            summary.discarded_outputs += 1;
            continue;
        }
        let file = open_file(&root, &path)?;
        if !same_file(&metadata, &safe_metadata(&file)?) {
            return Err(ToolError::ChangedInput);
        }
        let content = read_regular(file, MAX_FILE_BYTES)?;
        budget.bytes = budget
            .bytes
            .checked_add(content.len())
            .ok_or(ToolError::Limit)?;
        retained += 1;
        if budget.bytes > MAX_SNAPSHOT_BYTES || retained > MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        after.entries.insert(
            path.clone(),
            Entry {
                path,
                content: Some(content),
                executable,
            },
        );
    }
    let changeset = compare_command_snapshots(before, &after)?;
    selector.check_time()?;
    Ok((changeset, summary))
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
            ("omitted", serde_json::json!(["../outside"]), false),
            ("omitted", serde_json::json!([""]), false),
            (
                "omitted",
                serde_json::json!([vec!["deep"; 17].join("/")]),
                true,
            ),
            ("controls", serde_json::json!({"../outside":"x"}), false),
            (
                "controls",
                serde_json::json!({vec!["deep"; 16].join("/"):"x"}),
                true,
            ),
            (
                "controls",
                serde_json::json!({"":"x".repeat(CONTROL_BYTES + 1)}),
                true,
            ),
            (
                "summary",
                serde_json::json!({"ignored_entries":0,"pruned_directories":1,"oversized_inputs":0,"discarded_outputs":0}),
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

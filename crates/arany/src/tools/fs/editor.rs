use super::command::{CommandSelection, command_metadata};
use super::tree::{self, Pair, Sorted};
use super::*;
use crate::session::rewind::FileState;
use crate::session::worker::EditorChange;
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::time::Instant;

#[cfg(test)]
mod tests;

pub(in crate::tools) const TREE: &str = "tree";
const BASE: &str = "base";
const READY: &str = "ready";
// One base record: a path within the Tool path bound, its digest and the clone identity.
const BASE_LINE_BYTES: usize = 4 * 1024;

// A populated input's version when the workspace was made, in component order in `base`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BaseFile {
    path: String,
    sha256: String,
    bytes: u64,
    executable: bool,
    // The tree file's inode and change time; while both hold, its content cannot have changed.
    inode: u64,
    changed: (i64, i64),
}

impl BaseFile {
    // Only files within the publisher's file bound can be integrated as a before version.
    fn state(&self) -> Result<FileState, ToolError> {
        Ok(FileState {
            sha256: self.sha256.clone(),
            bytes: integrable(self.bytes).map_err(|error| error.at_path(&self.path))?,
            executable: self.executable,
        })
    }
}

fn integrable(bytes: u64) -> Result<u32, ToolError> {
    u32::try_from(bytes)
        .ok()
        .filter(|bytes| *bytes as usize <= MAX_FILE_BYTES)
        .ok_or(ToolError::Limit)
}

fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

// Streams a regular file's SHA-256 and length.
fn hash(mut file: File) -> Result<(String, u64), ToolError> {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    let mut length = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|_| ToolError::Operation)?;
        if read == 0 {
            return Ok((crate::validate::hex(&digest.finalize().into()), length));
        }
        digest.update(&buffer[..read]);
        length += read as u64;
    }
}

// Read and copy granularity for streamed inputs.
const COPY_BUFFER_BYTES: usize = 64 * 1024;

// A byte copy may not take the volume holding the workspace below the free-space reserve.
fn require_reserve(directory: &Dir, bytes: u64) -> Result<(), ToolError> {
    let capacity = rustix::fs::fstatvfs(directory).map_err(|_| ToolError::Operation)?;
    let free = capacity.f_bavail.saturating_mul(capacity.f_frsize);
    if free >= GUARD_DISK_RESERVE_BYTES.saturating_add(bytes) {
        return Ok(());
    }
    Err(ToolError::DiskSpace(format!(
        "Not enough free disk space for an editor workspace: {} MiB free and Arany keeps {} MiB free. Free disk space, then retry.",
        free / BYTES_PER_MEBIBYTE,
        GUARD_DISK_RESERVE_BYTES / BYTES_PER_MEBIBYTE
    )))
}

// Clones one selected input of any size into the editor tree; another volume or filesystem falls back
// to a streamed copy within the disk reserve. Records the tree file's version.
pub(super) fn copy_input(
    root: &Dir,
    path: &str,
    mut source: File,
    metadata: &std::fs::Metadata,
) -> Result<BaseFile, ToolError> {
    let (parent, leaf) = create_parents(root, path)?;
    let mode = if executable(metadata) { 0o700 } else { 0o600 };
    #[cfg(target_os = "macos")]
    let cloned = crate::private_fs::clone_at(&source, &parent, &leaf).is_ok();
    #[cfg(target_os = "linux")]
    let cloned = false;
    let written = if cloned {
        use std::os::unix::fs::PermissionsExt;
        let file = open_file(&parent, &leaf)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|_| ToolError::Operation)?;
        file
    } else {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = parent
            .open_with(&leaf, &options)
            .map_err(|_| ToolError::Operation)?
            .into_std();
        #[cfg(target_os = "linux")]
        let reflinked = rustix::fs::ioctl_ficlone(&file, &source).is_ok();
        #[cfg(target_os = "macos")]
        let reflinked = false;
        if !reflinked {
            require_reserve(&parent, metadata.len())?;
            std::io::copy(&mut source, &mut file).map_err(|_| ToolError::Operation)?;
        }
        file
    };
    // Keeping the source time lets incremental builds such as Cargo treat unchanged inputs as fresh.
    written
        .set_modified(metadata.modified().map_err(|_| ToolError::Path)?)
        .map_err(|_| ToolError::Operation)?;
    let identity = written.metadata().map_err(|_| ToolError::Operation)?;
    let (sha256, bytes) = hash(open_file(&parent, &leaf)?)?;
    use std::os::unix::fs::MetadataExt;
    Ok(BaseFile {
        path: path.into(),
        sha256,
        bytes,
        executable: executable(metadata),
        inode: identity.ino(),
        changed: (identity.ctime(), identity.ctime_nsec()),
    })
}

// Fills an empty private editor directory: the tree, its frozen selection and base, then the ready marker.
pub(in crate::tools) fn create(
    directory: &Dir,
    workspace: &Dir,
    config: &Config,
    deadline: Instant,
) -> Result<(), ToolError> {
    let mut builder = DirBuilder::new();
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    directory
        .create_dir_with(TREE, &builder)
        .map_err(|_| ToolError::Operation)?;
    let target = directory
        .open_dir_nofollow(TREE)
        .map_err(|_| ToolError::Path)?;
    let mut base =
        super::command::select_editor_inputs(workspace, &target, directory, config, deadline)?;
    base.sort_by(|left, right| tree::order(&left.path, &right.path));
    let mut lines = Vec::new();
    for file in &base {
        serde_json::to_writer(&mut lines, file).map_err(|_| ToolError::Operation)?;
        lines.push(b'\n');
    }
    put(directory, BASE, &lines, false)?;
    put(directory, READY, b"", false)?;
    sync_directory(directory)
}

fn sync_directory(directory: &Dir) -> Result<(), ToolError> {
    directory
        .open(".")
        .and_then(|handle| handle.sync_all())
        .map_err(|_| ToolError::Operation)
}

pub(in crate::tools) fn ready(directory: &Dir) -> bool {
    directory
        .symlink_metadata(READY)
        .is_ok_and(|metadata| metadata.is_file())
}

pub(in crate::tools) struct ChangeLimits {
    pub(in crate::tools) files: usize,
    pub(in crate::tools) bytes: u64,
}

// The project changes an editor made: tree against base, under the frozen selection rules. Ignored,
// dependency and protected outputs never integrate: new ones are discarded, while one that would shadow
// an existing Workspace path refuses as `Conflicts` naming it. Unsafe entries refuse; overflow is `Limit`.
pub(in crate::tools) fn changes(
    directory: &Dir,
    workspace: &Dir,
    deadline: Instant,
    limits: &ChangeLimits,
) -> Result<Vec<EditorChange>, ToolError> {
    if !ready(directory) {
        return Err(ToolError::Path);
    }
    let selection = CommandSelection::read_dir(directory)?;
    let tree = directory
        .open_dir_nofollow(TREE)
        .map_err(|_| ToolError::Path)?;
    let mut capture = Capture {
        tree: &tree,
        workspace,
        selector: selection.selector(deadline)?,
        selection: &selection,
        limits,
        bytes: 0,
        changes: Vec::new(),
        shadowing: Vec::new(),
    };
    tree::diff(
        &mut BaseReader::new(open_file(directory, BASE)?),
        &mut tree::Walk::new(&tree, deadline)?,
        |pair| {
            let path = match &pair {
                Pair::Left(base) | Pair::Both(base, _) => base.path.clone(),
                Pair::Right(node) => node.path.clone(),
            };
            capture.visit(pair).map_err(|error| match error {
                ToolError::AtPath { .. } => error,
                error => error.at_path(&path),
            })
        },
    )?;
    capture.selector.check_time()?;
    if !capture.shadowing.is_empty() {
        let paths: Vec<_> = capture.shadowing.iter().map(String::as_str).collect();
        return Err(ToolError::Conflicts(conflict_list(&paths)));
    }
    let mut changes = capture.changes;
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

struct Capture<'a> {
    tree: &'a Dir,
    workspace: &'a Dir,
    shadowing: Vec<String>,
    selection: &'a CommandSelection,
    selector: crate::tools::selection::Selector,
    limits: &'a ChangeLimits,
    bytes: u64,
    changes: Vec<EditorChange>,
}

impl Capture<'_> {
    fn visit(&mut self, pair: Pair<BaseFile, tree::Node>) -> Result<bool, ToolError> {
        self.selector.check_time()?;
        let (base, node) = match pair {
            Pair::Left(base) => {
                let before = Some(base.state()?);
                return self.push(base.path, before, None).map(|()| true);
            }
            Pair::Right(node) => (None, node),
            Pair::Both(base, node) => (Some(base), node),
        };
        let path = node.path;
        if path.split('/').any(forbidden) && base.is_none() {
            self.shadows(path);
            return Ok(false);
        }
        if !valid_relative(&path, false) {
            return Err(ToolError::Path);
        }
        if path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
            return Err(ToolError::Limit);
        }
        let ignored = base.is_none()
            && (path.split('/').any(ignored_directory)
                || self.selection.prunes(&path)
                || self.selector.excluded(&path, node.metadata.is_dir())?);
        if ignored {
            self.shadows(path);
            return Ok(false);
        }
        if node.metadata.is_dir() {
            return if base.is_some() {
                Err(ToolError::Path)
            } else {
                Ok(true)
            };
        }
        let file = open_file(self.tree, &path)?;
        let metadata = command_metadata(&file)?;
        if let Some(base) = &base
            && unchanged(base, &metadata)
        {
            return Ok(true);
        }
        let executable = executable(&metadata);
        let (sha256, bytes) = hash(file)?;
        if base
            .as_ref()
            .is_some_and(|base| base.sha256 == sha256 && base.executable == executable)
        {
            return Ok(true);
        }
        let after = FileState {
            sha256,
            bytes: integrable(bytes).map_err(|error| error.at_path(&path))?,
            executable,
        };
        let before = base.map(|base| base.state()).transpose()?;
        self.push(path, before, Some(after)).map(|()| true)
    }

    // An excluded output over an existing Workspace path cannot integrate, and silence would hide it.
    fn shadows(&mut self, path: String) {
        if self.workspace.symlink_metadata(&path).is_ok() {
            self.shadowing.push(path);
        }
    }

    fn push(
        &mut self,
        path: String,
        before: Option<FileState>,
        after: Option<FileState>,
    ) -> Result<(), ToolError> {
        self.bytes = self
            .bytes
            .checked_add(after.as_ref().map_or(0, |after| u64::from(after.bytes)))
            .ok_or(ToolError::Limit)?;
        if self.changes.len() >= self.limits.files || self.bytes > self.limits.bytes {
            return Err(ToolError::Limit);
        }
        self.changes.push(EditorChange {
            path,
            before,
            after,
        });
        Ok(())
    }
}

fn unchanged(base: &BaseFile, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (metadata.ino(), (metadata.ctime(), metadata.ctime_nsec())) == (base.inode, base.changed)
}

// Streams `base` records, which must be strictly increasing in component order.
struct BaseReader {
    lines: std::io::BufReader<File>,
    last: Option<String>,
}

impl BaseReader {
    fn new(file: File) -> Self {
        Self {
            lines: std::io::BufReader::new(file),
            last: None,
        }
    }
}

impl Sorted for BaseReader {
    type Item = BaseFile;

    fn path(item: &BaseFile) -> &str {
        &item.path
    }

    fn next(&mut self) -> Result<Option<BaseFile>, ToolError> {
        let mut line = Vec::new();
        let read = Read::by_ref(&mut self.lines)
            .take(BASE_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| ToolError::Operation)?;
        if read == 0 {
            return Ok(None);
        }
        if line.pop() != Some(b'\n') {
            return Err(ToolError::Path);
        }
        let file: BaseFile = serde_json::from_slice(&line).map_err(|_| ToolError::Path)?;
        if !valid_relative(&file.path, false)
            || !valid_digest(&file.sha256)
            || self
                .last
                .as_deref()
                .is_some_and(|last| tree::order(last, &file.path).is_ge())
        {
            return Err(ToolError::Path);
        }
        self.last = Some(file.path.clone());
        Ok(Some(file))
    }

    fn skip(&mut self) {}
}

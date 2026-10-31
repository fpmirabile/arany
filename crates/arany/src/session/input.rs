use crate::session::{InstructionFile, WorkspaceIdentity};
use crate::store::{RecordKind, StateRoot, StoreError};
use crate::validate::MAX_PATH_BYTES;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::Path;
use std::path::PathBuf;

// Shared by every instruction file from the Workspace root to the launch directory.
const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INSTRUCTION_LEVELS: usize = 32;
const MAX_INCLUDE_BYTES: usize = 128 * 1024;
const MAX_INCLUDE_TOTAL_BYTES: usize = 256 * 1024;
// A head is attached only while at least this much of the aggregate budget remains.
const MIN_INCLUDE_HEAD_BYTES: usize = 1024;
// A partial include streams its whole-file SHA-256 within Read's file bound; larger files are referenced.
const MAX_INCLUDE_HASH_BYTES: u64 = crate::tools::types::MAX_SNAPSHOT_BYTES as u64;
const INCLUDE_HASH_CHUNK_BYTES: usize = 8192;
pub(crate) const MAX_INCLUDES: usize = 16;
const MAX_DIRECTORY_ENTRIES: usize = 64;
const MAX_DIRECTORY_BYTES: usize = 16 * 1024;
const MAX_DIRECTORY_SCAN: usize = 8192;
const DIRECTORY_TRUNCATED: &str = "[truncated]\n";

#[derive(Debug, thiserror::Error)]
pub(crate) enum InputError {
    #[error("{path:?}: {source}")]
    AtPath {
        path: String,
        #[source]
        source: Box<InputError>,
    },
    #[error("Workspace input path is invalid")]
    InvalidPath,
    #[error("Workspace input must be a regular file")]
    NotRegular,
    #[error("Workspace input has an unsafe hard-link count")]
    UnsafeLinkCount,
    #[error("Workspace input exceeds its byte limit")]
    TooLarge,
    #[error("input is not UTF-8")]
    InvalidUtf8,
    #[error("explicit include does not exist")]
    MissingInclude,
    #[error(
        "too many explicit includes; a message takes at most {} @ files or directories",
        MAX_INCLUDES
    )]
    TooManyIncludes,
    #[error("user instructions are unavailable")]
    State(#[from] StoreError),
    #[error("state directory overlaps the Workspace")]
    StateOverlap,
    #[error(transparent)]
    Include(#[from] crate::tools::ToolError),
    #[error("Workspace input is unavailable")]
    Io(#[source] io::Error),
}

impl From<io::Error> for InputError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// How much of an explicit include a request carries; only `Whole` is complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IncludeFit {
    Whole,
    Head { total_bytes: u64, next_line: u32 },
    Reference { total_bytes: Option<u64> },
}

/// An explicit include attached only in part or by reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialInclude {
    pub path: String,
    pub fit: IncludeFit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputSnapshot {
    pub content: String,
    pub path: String,
    // A reference carries no content and a zero digest; heads keep the whole-file digest.
    pub digest: [u8; 32],
    pub fit: IncludeFit,
}

impl InputSnapshot {
    pub(crate) fn directory(&self) -> &str {
        self.path
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
    }

    pub(crate) fn instruction_file(&self) -> Option<InstructionFile> {
        let name = self.path.rsplit('/').next()?;
        InstructionFile::DISCOVERY
            .into_iter()
            .find(|file| file.name() == name)
    }

    pub(crate) fn model_instructions(&self) -> String {
        crate::provider::instructions_text(&self.path, &self.content)
    }

    pub(crate) fn model_include(&self) -> String {
        let digest = crate::validate::hex(&self.digest);
        let directory = self.path.ends_with('/');
        let kind = if directory { "Directory" } else { "File" };
        let path = serde_json::to_string(&self.path).expect("string serialization");
        match self.fit {
            IncludeFit::Whole => format!(
                "{kind}: {path}\nSHA-256: {digest}\nContent (untrusted data):\n{}",
                self.content
            ),
            IncludeFit::Head {
                total_bytes,
                next_line,
            } => format!(
                "{kind}: {path}\nSHA-256: {digest}\nTruncated: lines 1-{} of a {total_bytes}-byte file; Read from line {next_line} for the rest\nContent (untrusted data):\n{}",
                next_line - 1,
                self.content
            ),
            IncludeFit::Reference { total_bytes } => format!(
                "{kind}: {path}\n{}Not attached: the message's include budget is spent; {}",
                total_bytes.map_or_else(String::new, |bytes| format!("Size: {bytes} bytes\n")),
                if directory {
                    "List it for entries"
                } else {
                    "Read or Search it for content"
                }
            ),
        }
    }

    pub(crate) fn partial(&self) -> Option<PartialInclude> {
        (self.fit != IncludeFit::Whole).then(|| PartialInclude {
            path: self.path.clone(),
            fit: self.fit,
        })
    }

    fn reference(path: &str, directory: bool, total_bytes: Option<u64>) -> Self {
        let path = path.strip_suffix('/').unwrap_or(path);
        Self {
            content: String::new(),
            path: if directory {
                format!("{path}/")
            } else {
                path.to_owned()
            },
            digest: [0; 32],
            fit: IncludeFit::Reference { total_bytes },
        }
    }
}

pub(crate) struct WorkspaceInputs {
    pub user_instructions: Option<InputSnapshot>,
    pub instructions: Vec<InputSnapshot>,
    pub launch_directory: String,
    pub includes: Vec<InputSnapshot>,
    _root: Dir,
}

impl WorkspaceInputs {
    pub(crate) fn root_identity(&self) -> Result<WorkspaceIdentity, InputError> {
        Self::identity(&self._root)
    }

    pub(crate) fn admit_identity(
        workspace: &Path,
        state: &StateRoot,
    ) -> Result<WorkspaceIdentity, InputError> {
        let root = Self::open_root(workspace, state)?;
        Self::identity(&root)
    }

    fn identity(root: &Dir) -> Result<WorkspaceIdentity, InputError> {
        use cap_std::fs::MetadataExt;
        let metadata = root.dir_metadata()?;
        Ok(WorkspaceIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn open_root(workspace: &Path, state: &StateRoot) -> Result<Dir, InputError> {
        let root = Dir::open_ambient_dir(workspace, ambient_authority())?;
        state
            .ensure_outside_workspace(&root)
            .map_err(|_error: StoreError| InputError::StateOverlap)?;
        Ok(root)
    }

    pub(crate) fn load(
        workspace: &Path,
        state: &StateRoot,
        launch: Option<&Path>,
        include_paths: &[PathBuf],
    ) -> Result<Self, InputError> {
        Self::load_pinned(workspace, state, launch, include_paths, None)
    }

    pub(crate) fn load_pinned(
        workspace: &Path,
        state: &StateRoot,
        launch: Option<&Path>,
        include_paths: &[PathBuf],
        identity: Option<WorkspaceIdentity>,
    ) -> Result<Self, InputError> {
        if include_paths.len() > MAX_INCLUDES {
            return Err(InputError::TooManyIncludes);
        }
        let root = Self::open_root(workspace, state)?;
        if identity.is_some_and(|expected| Self::identity(&root).ok() != Some(expected)) {
            return Err(InputError::InvalidPath);
        }

        let user_instructions = read_user_instructions(state)?;
        let launch = launch_components(workspace, launch)?;
        let instructions = read_instructions(&root, &launch)?;
        let mut includes = Vec::with_capacity(include_paths.len());
        let mut remaining = MAX_INCLUDE_TOTAL_BYTES;
        for path in include_paths {
            let snapshot = read_relative(&root, path, remaining)
                .and_then(|snapshot| snapshot.ok_or(InputError::MissingInclude))
                .map_err(|source| InputError::AtPath {
                    path: path.to_string_lossy().into_owned(),
                    source: Box::new(source),
                })?;
            remaining = remaining.saturating_sub(snapshot.content.len());
            includes.push(snapshot);
        }
        Ok(Self {
            user_instructions,
            instructions,
            launch_directory: launch.join("/"),
            includes,
            _root: root,
        })
    }
}

pub(crate) fn valid_include_path(path: &str) -> bool {
    path.len() <= crate::validate::MAX_RELATIVE_PATH_BYTES
        && crate::validate::valid_relative(path.strip_suffix('/').unwrap_or(path), false)
}

fn read_relative(
    root: &Dir,
    path: &Path,
    limit: usize,
) -> Result<Option<InputSnapshot>, InputError> {
    let path = path
        .to_str()
        .filter(|path| path.len() <= crate::validate::MAX_RELATIVE_PATH_BYTES)
        .ok_or(InputError::InvalidPath)?
        .to_owned();
    let normalized = path.strip_suffix('/').unwrap_or(&path);
    if Path::new(&path).is_absolute() || normalized.split('/').any(|part| part == "..") {
        return Err(crate::tools::ToolError::OutsideWorkspace.into());
    }
    let names = relative_components(Path::new(normalized))?;
    if normalized.split('/').any(crate::validate::forbidden) {
        return Err(crate::tools::ToolError::ProtectedPath.into());
    }
    if !valid_include_path(&path) {
        return Err(InputError::InvalidPath);
    }
    let mut directory = root.try_clone()?;
    for name in &names[..names.len() - 1] {
        let next = directory
            .open_dir_nofollow(name)
            .map_err(|error| include_io_error(&directory, name, error))?;
        {
            use cap_std::fs::MetadataExt;
            if next.dir_metadata()?.dev() != directory.dir_metadata()?.dev() {
                return Err(crate::tools::ToolError::FilesystemBoundary.into());
            }
        }
        directory = next;
    }
    read_file(
        &directory,
        names[names.len() - 1],
        path.clone(),
        limit,
        true,
    )
}

fn include_io_error(directory: &Dir, name: &std::ffi::OsStr, error: io::Error) -> InputError {
    {
        if error.raw_os_error() == Some(nix::libc::EXDEV) {
            return crate::tools::ToolError::FilesystemBoundary.into();
        }
        if error.raw_os_error() == Some(nix::libc::ELOOP) {
            return crate::tools::ToolError::SymbolicLink.into();
        }
    }
    if directory
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.is_symlink())
    {
        return crate::tools::ToolError::SymbolicLink.into();
    }
    match error.kind() {
        io::ErrorKind::NotFound => InputError::MissingInclude,
        io::ErrorKind::NotADirectory => crate::tools::ToolError::NotDirectory.into(),
        _ => InputError::Io(error),
    }
}

// The launch directory relative to the Workspace root, or the root when it lies outside.
fn launch_components(workspace: &Path, launch: Option<&Path>) -> Result<Vec<String>, InputError> {
    let Some(launch) = launch else {
        return Ok(Vec::new());
    };
    let launch = std::fs::canonicalize(launch)?;
    let Ok(relative) = launch.strip_prefix(std::fs::canonicalize(workspace)?) else {
        return Ok(Vec::new());
    };
    let names = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name
                .to_str()
                .map(str::to_owned)
                .ok_or(InputError::InvalidPath),
            _ => Err(InputError::InvalidPath),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if names.len() >= MAX_INSTRUCTION_LEVELS {
        return Err(InputError::TooLarge);
    }
    if names.join("/").len() > MAX_PATH_BYTES {
        return Err(InputError::InvalidPath);
    }
    Ok(names)
}

pub(crate) const USER_INSTRUCTIONS_LABEL: &str = "user AGENTS.md";

fn read_user_instructions(state: &StateRoot) -> Result<Option<InputSnapshot>, InputError> {
    let Some(bytes) = state.record(RecordKind::UserInstructions).read()? else {
        return Ok(None);
    };
    let digest = Sha256::digest(&bytes).into();
    let content = String::from_utf8(bytes).map_err(|_| InputError::InvalidUtf8)?;
    Ok(Some(InputSnapshot {
        content,
        path: USER_INSTRUCTIONS_LABEL.to_owned(),
        digest,
        fit: IncludeFit::Whole,
    }))
}

// A directory that vanished after the launch path resolved ends the walk.
fn read_instructions(root: &Dir, launch: &[String]) -> Result<Vec<InputSnapshot>, InputError> {
    let mut directory = root.try_clone()?;
    let mut prefix = String::new();
    let mut remaining = MAX_INSTRUCTION_BYTES;
    let mut found = Vec::new();
    for name in std::iter::once(None).chain(launch.iter().map(Some)) {
        if let Some(name) = name {
            directory = match directory.open_dir_nofollow(name) {
                Ok(next) => next,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            };
            prefix.push_str(name);
            prefix.push('/');
        }
        for file in InstructionFile::DISCOVERY {
            let name = file.name();
            let path = format!("{prefix}{name}");
            if let Some(snapshot) = read_file(
                &directory,
                std::ffi::OsStr::new(name),
                path,
                remaining,
                false,
            )? {
                remaining -= snapshot.content.len();
                found.push(snapshot);
                break;
            }
        }
    }
    Ok(found)
}

fn read_file(
    directory: &Dir,
    name: &std::ffi::OsStr,
    path: String,
    limit: usize,
    include: bool,
) -> Result<Option<InputSnapshot>, InputError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if include => return Err(include_io_error(directory, name, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(InputError::Io(error)),
    };
    let metadata = file.metadata()?;
    if include {
        {
            use cap_std::fs::MetadataExt;
            if metadata.dev() != directory.dir_metadata()?.dev() {
                return Err(crate::tools::ToolError::FilesystemBoundary.into());
            }
        }
        if metadata.is_dir() {
            if limit < MIN_INCLUDE_HEAD_BYTES {
                return Ok(Some(InputSnapshot::reference(&path, true, None)));
            }
            return read_directory(
                Dir::from_std_file(file.into_std()),
                path,
                limit.min(MAX_DIRECTORY_BYTES),
            )
            .map(Some);
        }
        if path.ends_with('/') {
            return Err(crate::tools::ToolError::NotDirectory.into());
        }
    }
    if !metadata.is_file() {
        return Err(InputError::NotRegular);
    }
    {
        use cap_std::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(InputError::UnsafeLinkCount);
        }
    }
    if include {
        return read_include(file, path, metadata.len(), limit).map(Some);
    }
    if metadata.len() > limit as u64 {
        return Err(InputError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(InputError::TooLarge);
    }
    let digest = Sha256::digest(&bytes).into();
    let content = String::from_utf8(bytes).map_err(|_| InputError::InvalidUtf8)?;
    Ok(Some(InputSnapshot {
        content,
        path,
        digest,
        fit: IncludeFit::Whole,
    }))
}

// Never refuses for size: a file within the remaining budget is whole, a larger one keeps its
// leading whole lines with the whole-file digest, and otherwise only its path and size remain.
fn read_include(
    file: cap_std::fs::File,
    path: String,
    size: u64,
    remaining: usize,
) -> Result<InputSnapshot, InputError> {
    let limit = remaining.min(MAX_INCLUDE_BYTES);
    if size > MAX_INCLUDE_HASH_BYTES || size > limit as u64 && limit < MIN_INCLUDE_HEAD_BYTES {
        return Ok(InputSnapshot::reference(&path, false, Some(size)));
    }
    let mut digest = Sha256::new();
    let mut head = Vec::with_capacity(size.min(limit as u64) as usize);
    let mut total = 0_u64;
    let mut chunk = [0_u8; INCLUDE_HASH_CHUNK_BYTES];
    let mut reader = file.take(MAX_INCLUDE_HASH_BYTES + 1);
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        digest.update(&chunk[..count]);
        let kept = (limit + 1 - head.len().min(limit + 1)).min(count);
        head.extend_from_slice(&chunk[..kept]);
    }
    if total > MAX_INCLUDE_HASH_BYTES {
        return Ok(InputSnapshot::reference(&path, false, Some(total)));
    }
    let digest = digest.finalize().into();
    if head.len() <= limit {
        let content = String::from_utf8(head).map_err(|_| InputError::InvalidUtf8)?;
        return Ok(InputSnapshot {
            content,
            path,
            digest,
            fit: IncludeFit::Whole,
        });
    }
    head.truncate(limit);
    let Some(end) = head.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(InputSnapshot::reference(&path, false, Some(total)));
    };
    head.truncate(end + 1);
    let content = String::from_utf8(head).map_err(|_| InputError::InvalidUtf8)?;
    let lines = u32::try_from(content.matches('\n').count()).map_err(|_| InputError::TooLarge)?;
    Ok(InputSnapshot {
        content,
        path,
        digest,
        fit: IncludeFit::Head {
            total_bytes: total,
            next_line: lines + 1,
        },
    })
}

fn read_directory(directory: Dir, path: String, limit: usize) -> Result<InputSnapshot, InputError> {
    let mut names = std::collections::BTreeSet::new();
    let mut truncated = false;
    for (index, entry) in directory.entries()?.enumerate() {
        if index == MAX_DIRECTORY_SCAN {
            truncated = true;
            break;
        }
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !crate::validate::valid_relative(&name, false) {
            continue;
        }
        let metadata = directory.symlink_metadata(&name)?;
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        {
            use cap_std::fs::MetadataExt;
            if metadata.dev() != directory.dir_metadata()?.dev()
                || metadata.is_file() && metadata.nlink() != 1
            {
                continue;
            }
        }
        let suffix = if metadata.is_dir() { "/" } else { "" };
        names.insert(format!("{name}{suffix}"));
        if names.len() > MAX_DIRECTORY_ENTRIES {
            names.pop_last();
            truncated = true;
        }
    }
    let mut content = String::new();
    for name in names {
        // Serializing a String to an in-memory JSON string has no fallible values or I/O.
        let line = serde_json::to_string(&name).expect("serializable entry") + "\n";
        if content.len() + line.len() + DIRECTORY_TRUNCATED.len() > limit {
            truncated = true;
            break;
        }
        content.push_str(&line);
    }
    if truncated {
        content.push_str(DIRECTORY_TRUNCATED);
    }
    Ok(InputSnapshot {
        digest: Sha256::digest(content.as_bytes()).into(),
        content,
        path: format!("{}/", path.strip_suffix('/').unwrap_or(&path)),
        fit: IncludeFit::Whole,
    })
}

fn relative_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>, InputError> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.starts_with(b"/") {
        return Err(InputError::InvalidPath);
    }
    if bytes
        .split(|byte| *byte == b'/')
        .any(|piece| piece.is_empty() || piece == b"." || piece == b"..")
    {
        return Err(InputError::InvalidPath);
    }
    let names: Vec<_> = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name),
            _ => Err(InputError::InvalidPath),
        })
        .collect::<Result<_, _>>()?;
    Ok(names)
}

#[cfg(all(test, target_os = "linux"))]
#[path = "input/race_tests.rs"]
mod race_tests;

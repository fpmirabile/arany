use crate::session::{InstructionFile, WorkspaceIdentity};
use crate::store::{StateRoot, StoreError};
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
// Entries scanned to confirm an instruction file's exact spelling.
const MAX_DIRECTORY_ENTRIES: usize = 1 << 16;
const MAX_INCLUDE_BYTES: usize = 128 * 1024;
const MAX_INCLUDE_TOTAL_BYTES: usize = 256 * 1024;
const MAX_INCLUDES: usize = 16;

#[derive(Debug, thiserror::Error)]
pub(crate) enum InputError {
    #[error("Workspace input path is invalid")]
    InvalidPath,
    #[error("Workspace input must be a regular file")]
    NotRegular,
    #[error("Workspace input has an unsafe hard-link count")]
    UnsafeLinkCount,
    #[error("Workspace input exceeds its byte limit")]
    TooLarge,
    #[error("Workspace input is not UTF-8")]
    InvalidUtf8,
    #[error("explicit include does not exist")]
    MissingInclude,
    #[error("too many explicit includes")]
    TooManyIncludes,
    #[error("state directory overlaps the Workspace")]
    StateOverlap,
    #[error("Workspace input is unavailable")]
    Io(#[source] io::Error),
}

impl From<io::Error> for InputError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputSnapshot {
    pub content: String,
    pub path: String,
    pub digest: [u8; 32],
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
        let digest: String = self
            .digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!(
            "File: {}\nSHA-256: {digest}\nContent (untrusted data):\n{}",
            serde_json::to_string(&self.path).expect("string serialization"),
            self.content
        )
    }
}

pub(crate) struct WorkspaceInputs {
    pub instructions: Vec<InputSnapshot>,
    pub launch_directory: String,
    pub includes: Vec<InputSnapshot>,
    _root: Dir,
}

impl WorkspaceInputs {
    #[cfg(unix)]
    pub(crate) fn root_identity(&self) -> Result<WorkspaceIdentity, InputError> {
        Self::identity(&self._root)
    }

    #[cfg(unix)]
    pub(crate) fn admit_identity(
        workspace: &Path,
        state: &StateRoot,
    ) -> Result<WorkspaceIdentity, InputError> {
        let root = Self::open_root(workspace, state)?;
        Self::identity(&root)
    }

    #[cfg(unix)]
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
        #[cfg(unix)]
        state
            .ensure_outside_workspace(&root)
            .map_err(|_error: StoreError| InputError::StateOverlap)?;
        #[cfg(not(unix))]
        return Err(InputError::InvalidPath);
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

        let launch = launch_components(workspace, launch)?;
        let instructions = read_instructions(&root, &launch)?;
        let mut includes = Vec::with_capacity(include_paths.len());
        let mut total_bytes = 0_usize;
        for path in include_paths {
            let snapshot =
                read_relative(&root, path, MAX_INCLUDE_BYTES)?.ok_or(InputError::MissingInclude)?;
            total_bytes = total_bytes
                .checked_add(snapshot.content.len())
                .ok_or(InputError::TooLarge)?;
            if total_bytes > MAX_INCLUDE_TOTAL_BYTES {
                return Err(InputError::TooLarge);
            }
            includes.push(snapshot);
        }
        Ok(Self {
            instructions,
            launch_directory: launch.join("/"),
            includes,
            _root: root,
        })
    }
}

fn read_relative(
    root: &Dir,
    path: &Path,
    limit: usize,
) -> Result<Option<InputSnapshot>, InputError> {
    let names = relative_components(path)?;
    let path = path
        .to_str()
        .filter(|path| path.len() <= 4096)
        .ok_or(InputError::InvalidPath)?
        .to_owned();
    let mut directory = root.try_clone()?;
    for name in &names[..names.len() - 1] {
        directory = directory.open_dir_nofollow(name)?;
    }
    read_file(&directory, names[names.len() - 1], path, limit)
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
    if names.join("/").len() > 4096 {
        return Err(InputError::InvalidPath);
    }
    Ok(names)
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
            let Some(opened) = open_nofollow(&directory, std::ffi::OsStr::new(name))? else {
                continue;
            };
            let metadata = opened.metadata()?;
            #[cfg(test)]
            entry_hook();
            if !exact_entry(&directory, name, &metadata)? {
                continue;
            }
            let snapshot = snapshot(opened, &metadata, format!("{prefix}{name}"), remaining)?;
            remaining -= snapshot.content.len();
            found.push(snapshot);
            break;
        }
    }
    Ok(found)
}

// Case-insensitive volumes open `agents.md` for `AGENTS.md`; only the exact, unreplaced entry counts.
fn exact_entry(
    directory: &Dir,
    name: &str,
    opened: &cap_std::fs::Metadata,
) -> Result<bool, InputError> {
    for (index, entry) in directory.entries()?.enumerate() {
        if index == MAX_DIRECTORY_ENTRIES {
            return Err(InputError::TooLarge);
        }
        let entry = entry?;
        if entry.file_name().as_os_str() != std::ffi::OsStr::new(name) {
            continue;
        }
        // The readdir record's inode, never a fresh lookup of a name that may have moved.
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            use std::os::unix::fs::DirEntryExt;
            let listed = entry.ino();
            #[cfg(test)]
            entry_hook();
            if (directory.dir_metadata()?.dev(), listed) != (opened.dev(), opened.ino()) {
                return Err(InputError::Io(io::Error::other(
                    "instruction file was replaced",
                )));
            }
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
thread_local! {
    // Runs after an instruction file opens and again once its directory entry is listed.
    pub(crate) static ENTRY_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn entry_hook() {
    ENTRY_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

fn read_file(
    directory: &Dir,
    name: &std::ffi::OsStr,
    path: String,
    limit: usize,
) -> Result<Option<InputSnapshot>, InputError> {
    let Some(file) = open_nofollow(directory, name)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    snapshot(file, &metadata, path, limit).map(Some)
}

fn open_nofollow(
    directory: &Dir,
    name: &std::ffi::OsStr,
) -> Result<Option<cap_std::fs::File>, InputError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    match directory.open_with(name, &options) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(InputError::Io(error)),
    }
}

fn snapshot(
    file: cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
    path: String,
    limit: usize,
) -> Result<InputSnapshot, InputError> {
    if !metadata.is_file() {
        return Err(InputError::NotRegular);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(InputError::UnsafeLinkCount);
        }
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
    Ok(InputSnapshot {
        content,
        path,
        digest,
    })
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn relative_components(_path: &Path) -> Result<Vec<&std::ffi::OsStr>, InputError> {
    Err(InputError::InvalidPath)
}

#[cfg(all(test, target_os = "linux"))]
#[path = "input/race_tests.rs"]
mod race_tests;

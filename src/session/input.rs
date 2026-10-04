use crate::store::{StateRoot, StoreError};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::Path;
use std::path::PathBuf;

const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstructionSource {
    Agents,
    Claude,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputSnapshot {
    pub content: String,
    pub digest: [u8; 32],
}

pub(crate) struct WorkspaceInputs {
    pub instructions: Option<(InstructionSource, InputSnapshot)>,
    pub includes: Vec<InputSnapshot>,
    _root: Dir,
}

impl WorkspaceInputs {
    #[cfg(unix)]
    pub(crate) fn root_identity(&self) -> Result<(u64, u64), InputError> {
        Self::identity(&self._root)
    }

    #[cfg(unix)]
    pub(crate) fn admit_identity(
        workspace: &Path,
        state: &StateRoot,
    ) -> Result<(u64, u64), InputError> {
        let root = Self::open_root(workspace, state)?;
        Self::identity(&root)
    }

    #[cfg(unix)]
    fn identity(root: &Dir) -> Result<(u64, u64), InputError> {
        use cap_std::fs::MetadataExt;
        let metadata = root.dir_metadata()?;
        Ok((metadata.dev(), metadata.ino()))
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
        include_paths: &[PathBuf],
    ) -> Result<Self, InputError> {
        if include_paths.len() > MAX_INCLUDES {
            return Err(InputError::TooManyIncludes);
        }
        let root = Self::open_root(workspace, state)?;

        let instructions =
            match read_relative(&root, Path::new("AGENTS.md"), MAX_INSTRUCTION_BYTES)? {
                Some(snapshot) => Some((InstructionSource::Agents, snapshot)),
                None => read_relative(&root, Path::new("CLAUDE.md"), MAX_INSTRUCTION_BYTES)?
                    .map(|snapshot| (InstructionSource::Claude, snapshot)),
            };
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
    let mut directory = root.try_clone()?;
    for name in &names[..names.len() - 1] {
        directory = directory.open_dir_nofollow(name)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = match directory.open_with(names[names.len() - 1], &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(InputError::Io(error)),
    };
    let metadata = file.metadata()?;
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
    Ok(Some(InputSnapshot { content, digest }))
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

use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirBuilder};
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod acl;

#[derive(Debug)]
pub(crate) enum Error {
    InvalidPath,
    NotPrivate,
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) struct PrivateDirectory {
    pub path: PathBuf,
    pub dir: Dir,
    #[cfg(unix)]
    pub ancestors: Vec<(u64, u64)>,
}

pub(crate) fn open_workspace_directory(path: &Path) -> Result<Dir, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidPath);
    }
    #[cfg(target_os = "macos")]
    {
        macos_directory(path, rustix::fs::OFlags::RDONLY)
    }
    #[cfg(target_os = "linux")]
    {
        let mut dir = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
        for component in path.components().skip(1) {
            let Component::Normal(name) = component else {
                return Err(Error::InvalidPath);
            };
            dir = dir.open_dir_nofollow(name)?;
        }
        Ok(dir)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(Error::InvalidPath)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_directory(path: &Path, final_access: rustix::fs::OFlags) -> Result<Dir, Error> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::fs::File;

    let common = OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW;
    let search = OFlags::from_bits_retain(nix::libc::O_SEARCH as _);
    let mut components = path.components().skip(1).peekable();
    let access = if components.peek().is_none() {
        final_access
    } else {
        search
    };
    let mut file = File::from(
        open("/", common | access, Mode::empty())
            .map_err(|error| Error::Io(io::Error::from(error)))?,
    );
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(Error::InvalidPath);
        };
        let access = if components.peek().is_none() {
            final_access
        } else {
            search
        };
        file = File::from(
            openat(&file, name, common | access, Mode::empty())
                .map_err(|error| Error::Io(io::Error::from(error)))?,
        );
    }
    Ok(Dir::from_std_file(file))
}

pub(crate) fn open_directory(path: &Path, create: bool) -> Result<PrivateDirectory, Error> {
    open_directory_checked(path, create, || Ok(()))
}

pub(crate) fn open_directory_checked(
    path: &Path,
    create: bool,
    check: impl Fn() -> Result<(), Error>,
) -> Result<PrivateDirectory, Error> {
    check()?;
    if !path.is_absolute() {
        return Err(Error::InvalidPath);
    }
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(Error::InvalidPath);
    }
    let mut names = Vec::new();
    let mut normalized = PathBuf::from("/");
    for component in components {
        let Component::Normal(name) = component else {
            return Err(Error::InvalidPath);
        };
        names.push(name);
        normalized.push(name);
    }
    if names.is_empty() || normalized != path {
        return Err(Error::InvalidPath);
    }
    let mut dir = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
    #[cfg(unix)]
    let mut ancestors = vec![identity(&dir)?];
    for (index, name) in names.iter().enumerate() {
        check()?;
        match dir.open_dir_nofollow(name) {
            Ok(next) => dir = next,
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                #[cfg(unix)]
                {
                    use cap_std::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match dir.create_dir_with(name, &builder) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(Error::Io(error)),
                }
                dir = dir.open_dir_nofollow(name)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
        check_directory(&dir, index + 1 == names.len())?;
        #[cfg(unix)]
        ancestors.push(identity(&dir)?);
    }
    Ok(PrivateDirectory {
        path: normalized,
        dir,
        #[cfg(unix)]
        ancestors,
    })
}

#[cfg(unix)]
fn identity(dir: &Dir) -> Result<(u64, u64), Error> {
    use cap_std::fs::MetadataExt;
    let metadata = dir.dir_metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
pub(crate) fn check_directory(dir: &Dir, leaf: bool) -> Result<(), Error> {
    check_acl(dir, leaf)?;
    use cap_std::fs::MetadataExt;
    let metadata = dir.dir_metadata()?;
    let mode = metadata.mode();
    let current_uid = rustix::process::geteuid().as_raw();
    if !metadata.is_dir() || (metadata.uid() != current_uid && metadata.uid() != 0) {
        return Err(Error::NotPrivate);
    }
    if leaf {
        if metadata.uid() != current_uid || mode & 0o077 != 0 {
            return Err(Error::NotPrivate);
        }
    } else if mode & 0o022 != 0 && !(metadata.uid() == 0 && mode & 0o1000 != 0) {
        return Err(Error::NotPrivate);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_directory(_dir: &Dir, _leaf: bool) -> Result<(), Error> {
    Err(Error::InvalidPath)
}

#[cfg(unix)]
pub(crate) fn check_acl(file: &impl std::os::fd::AsFd, private: bool) -> Result<(), Error> {
    #[cfg(target_os = "macos")]
    {
        acl::check(file.as_fd(), private)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (file, private);
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (file, private);
        Err(Error::NotPrivate)
    }
}

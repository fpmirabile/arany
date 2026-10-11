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

pub(crate) fn open_directory(path: &Path, create: bool) -> Result<PrivateDirectory, Error> {
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
        match dir.open_dir_nofollow(name) {
            Ok(next) => dir = next,
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                #[cfg(unix)]
                {
                    use cap_std::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                dir.create_dir_with(name, &builder)?;
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
fn check_directory(dir: &Dir, leaf: bool) -> Result<(), Error> {
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
fn check_directory(_dir: &Dir, _leaf: bool) -> Result<(), Error> {
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

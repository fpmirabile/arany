use super::*;
use cap_std::fs::{Dir, Metadata, MetadataExt};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;
use std::time::Instant;

const MASK_ARGUMENT_BYTES: usize = MAX_PATH_BYTES / 2;

#[derive(Default)]
pub(super) struct Inventory {
    masks: BTreeMap<PathBuf, Metadata>,
    protected: BTreeSet<(u64, u64)>,
    aliases: BTreeSet<(u64, u64)>,
    links: BTreeMap<PathBuf, PathBuf>,
    budget: crate::tools::fs::CommandScanBudget,
}

impl Inventory {
    #[cfg(test)]
    pub(super) fn paths(&self) -> Vec<PathBuf> {
        self.masks.keys().cloned().collect()
    }

    pub(super) fn read(lower: &Dir, deadline: Instant) -> Result<Self, ToolError> {
        let mut inventory = Self::default();
        if inventory.walk(lower, Path::new(""), deadline).is_err() {
            if Instant::now() >= deadline {
                return Err(ToolError::ProtectionUnavailable);
            }
            inventory.mask(lower, Path::new(""))?;
        }
        if !inventory.aliases.is_disjoint(&inventory.protected) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let paths: Vec<_> = inventory.masks.keys().collect();
        if serde_json::to_vec(&paths)
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .len()
            > MASK_ARGUMENT_BYTES
        {
            inventory.mask(lower, Path::new(""))?;
        }
        Ok(inventory)
    }

    fn mask(&mut self, lower: &Dir, path: &Path) -> Result<(), ToolError> {
        let metadata = if path.as_os_str().is_empty() {
            lower.dir_metadata()
        } else {
            lower.symlink_metadata(path)
        }
        .map_err(|_| ToolError::ProtectionUnavailable)?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(ToolError::ProtectionUnavailable);
        }
        self.masks.retain(|child, _| !child.starts_with(path));
        self.masks.insert(path.to_owned(), metadata);
        Ok(())
    }

    fn walk(&mut self, lower: &Dir, path: &Path, deadline: Instant) -> Result<(), ToolError> {
        let directory = match open(
            lower,
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        ) {
            Ok(directory) => directory,
            Err(_) => return self.mask(lower, path),
        };
        let directory = Dir::from_std_file(directory);
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(_) => return self.mask(lower, path),
        };
        for entry in entries {
            if Instant::now() >= deadline {
                return Err(ToolError::ProtectionUnavailable);
            }
            let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
            let name = entry.file_name();
            let child = path.join(&name);
            let Some(text) = child.to_str() else {
                return self.mask(lower, path);
            };
            self.budget.entry(text)?;
            let metadata = directory
                .symlink_metadata(&name)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            let protected = name.to_str().is_none_or(crate::validate::forbidden);
            if protected
                || metadata.mode() & 0o6000 != 0
                || (!metadata.is_dir() && !metadata.is_file() && !metadata.is_symlink())
            {
                self.protect(lower, &child, &metadata)?;
            } else if metadata.is_file() && metadata.nlink() > 1 {
                self.aliases.insert((metadata.dev(), metadata.ino()));
            } else if metadata.is_dir() {
                if child.components().count() >= crate::tools::types::COMMAND_SNAPSHOT_DEPTH {
                    self.mask(lower, &child)?;
                } else if self.walk(lower, &child, deadline).is_err() {
                    return Err(ToolError::ProtectionUnavailable);
                }
            }
        }
        Ok(())
    }

    fn protect(&mut self, lower: &Dir, path: &Path, metadata: &Metadata) -> Result<(), ToolError> {
        self.protected.insert((metadata.dev(), metadata.ino()));
        if metadata.is_symlink() {
            let target = match lower.canonicalize(path) {
                Ok(target) if !target.is_absolute() => target,
                _ => {
                    return self.mask(
                        lower,
                        path.parent().ok_or(ToolError::ProtectionUnavailable)?,
                    );
                }
            };
            self.budget
                .entry(target.to_str().ok_or(ToolError::ProtectionUnavailable)?)?;
            let pinned = lower
                .symlink_metadata(&target)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            self.protected.insert((pinned.dev(), pinned.ino()));
            self.mask(lower, &target)?;
            self.links.insert(path.to_owned(), target);
            self.mask(
                lower,
                path.parent().ok_or(ToolError::ProtectionUnavailable)?,
            )
        } else if metadata.is_dir() || metadata.is_file() {
            self.mask(lower, path)
        } else {
            self.mask(
                lower,
                path.parent().ok_or(ToolError::ProtectionUnavailable)?,
            )
        }
    }

    pub(super) fn verify(&self, lower: &Path) -> Result<(), ToolError> {
        let directory = crate::tools::fs::open_directory(lower)?;
        for (path, expected) in &self.masks {
            let file = open(&directory, path, rustix::fs::OFlags::PATH)?;
            let current = file
                .metadata()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if (expected.dev(), expected.ino(), expected.mode())
                != (current.dev(), current.ino(), current.mode())
            {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        for (link, target) in &self.links {
            if directory.canonicalize(link).as_ref().ok() != Some(target) {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        Ok(())
    }

    pub(super) fn install(&self, root: &Path) -> Result<Vec<PathBuf>, ToolError> {
        self.verify(&root.join("lower"))?;
        let merged = crate::tools::fs::open_directory(&root.join("merged"))?;
        let mut paths = Vec::new();
        for (path, metadata) in &self.masks {
            if paths.iter().any(|parent| path.starts_with(parent)) {
                continue;
            }
            let target = open(&merged, path, rustix::fs::OFlags::PATH)?;
            let current = target
                .metadata()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if current.is_dir() != metadata.is_dir() || (!current.is_dir() && !current.is_file()) {
                return Err(ToolError::ProtectionUnavailable);
            }
            let pinned = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&target));
            let source = root.join(if metadata.is_dir() {
                "empty-directory"
            } else {
                "empty-file"
            });
            unavailable(rustix::mount::mount_bind(&source, &pinned))?;
            let mounted_root = crate::tools::fs::open_directory(&root.join("merged"))?;
            let mounted = open(&mounted_root, path, rustix::fs::OFlags::PATH)?;
            let pinned = format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(&mounted)
            );
            unavailable(rustix::mount::mount_remount(
                &pinned,
                MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
                "",
            ))?;
            paths.push(path.clone());
        }
        Ok(paths)
    }
}

fn open(root: &Dir, path: &Path, flags: rustix::fs::OFlags) -> Result<std::fs::File, ToolError> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    unavailable(openat2(
        root,
        path,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    ))
    .map(std::fs::File::from)
}

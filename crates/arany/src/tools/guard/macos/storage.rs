use super::super::GuardCheck;
use crate::private_fs::open_directory;
use crate::tools::toolchains::RuntimeGrant;
use crate::tools::types::{BYTES_PER_MEBIBYTE, GUARD_DISK_RESERVE_BYTES};
use crate::tools::{ToolError, ToolRuntime};
use cap_std::fs::MetadataExt as _;
use std::path::Path;

const STORAGE_NAMES: [&str; 3] = ["workspace", "scratch", "cache"];

pub(super) use super::super::admission::{read_record, write_record};

pub(super) fn free_bytes(directory: &impl std::os::fd::AsFd) -> Result<u64, ToolError> {
    let capacity = rustix::fs::fstatvfs(directory).map_err(|_| ToolError::ProtectionUnavailable)?;
    capacity
        .f_bavail
        .checked_mul(capacity.f_frsize)
        .ok_or(ToolError::ProtectionUnavailable)
}

// Refuses a unit before launch when its volume is already at the reserve, naming both amounts.
pub(super) fn require_disk_reserve(directory: &impl std::os::fd::AsFd) -> Result<(), ToolError> {
    reserve_refusal(free_bytes(directory)?)
}

fn reserve_refusal(free: u64) -> Result<(), ToolError> {
    if free >= GUARD_DISK_RESERVE_BYTES {
        return Ok(());
    }
    Err(ToolError::DiskSpace(format!(
        "Not enough free disk space for a guarded command: {} MiB free on the disk holding {}, and Arany keeps {} MiB free. Free disk space, then retry.",
        free / BYTES_PER_MEBIBYTE,
        super::unit::UNIT_ROOT_PARENT,
        GUARD_DISK_RESERVE_BYTES / BYTES_PER_MEBIBYTE
    )))
}

#[cfg(test)]
pub(super) fn prepare_storage(root: &Path) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    prepare_volumes_with(root, &["workspace", "scratch"], None)?
        .try_into()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

// Unit storage is owner-only directories beside the unit root; Seatbelt confines writes and the disk floor bounds growth.
// A cloned cache view already exists: the controller made it before launch, so it is verified, not created.
fn prepare_volumes_with(
    root: &Path,
    names: &[&str],
    existing: Option<&str>,
) -> Result<Vec<cap_std::fs::Dir>, ToolError> {
    use cap_fs_ext::DirExt;

    if names.is_empty() || names.iter().any(|name| !STORAGE_NAMES.contains(name)) {
        return Err(ToolError::Configuration);
    }
    open_directory(root, false).map_err(|_| ToolError::Path)?;
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    let mut volumes = Vec::with_capacity(names.len());
    for name in names {
        if existing != Some(name) {
            match directory.symlink_metadata(name) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(ToolError::ChangedInput),
            }
            let mut builder = cap_std::fs::DirBuilder::new();
            cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
            directory
                .create_dir_with(name, &builder)
                .map_err(|_| ToolError::Operation)?;
        }
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
        volumes.push(volume);
    }
    Ok(volumes)
}

fn verify_volume(volume: &cap_std::fs::Dir, parent_device: u64) -> Result<(), ToolError> {
    let metadata = volume.dir_metadata().map_err(|_| ToolError::Path)?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.dev() != parent_device
        || metadata.mode() & 0o7777 != 0o700
        || crate::private_fs::check_acl(volume, true).is_err()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn command_volume_names(limits: &crate::tools::types::ToolLimits) -> &'static [&'static str] {
    if limits.cache_bytes > 0 {
        &STORAGE_NAMES
    } else {
        &STORAGE_NAMES[..2]
    }
}

pub(super) fn verify_storage(
    root: &Path,
    limits: &crate::tools::types::ToolLimits,
) -> Result<(), ToolError> {
    verify_volumes(root, command_volume_names(limits))
}

fn verify_volumes(root: &Path, names: &[&str]) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;

    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in names {
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
    }
    Ok(())
}

pub(super) fn prepare_command_storage(
    root: &Path,
    runtime: &ToolRuntime,
    limits: &crate::tools::types::ToolLimits,
    command: bool,
    cloned_cache: bool,
) -> Result<Vec<cap_std::fs::Dir>, ToolError> {
    use cap_fs_ext::DirExt;

    runtime.config.validate()?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    if crate::tools::fs::workspace_identity(&workspace)? != runtime.workspace_identity {
        return Err(ToolError::ChangedInput);
    }
    for grant in &runtime.config.runtime_grants {
        grant.verify_host()?;
    }
    crate::tools::fs::reject_overlap(root, &runtime.workspace)?;
    let volumes = prepare_volumes_with(
        root,
        command_volume_names(limits),
        cloned_cache.then_some("cache"),
    )?;
    let cache_volume = volumes
        .get(if limits.cache_bytes > 0 { 2 } else { 1 })
        .ok_or(ToolError::Operation)?;
    prepare_cache_links(
        root,
        cache_volume,
        &runtime.config.runtime_grants,
        cloned_cache,
    )?;
    let directory = crate::tools::fs::open_directory(root)?;
    let mut builder = cap_std::fs::DirBuilder::new();
    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
    directory
        .create_dir_with("runtime", &builder)
        .map_err(|_| ToolError::Operation)?;
    let resources = directory
        .open_dir_nofollow("runtime")
        .map_err(|_| ToolError::Path)?;
    crate::tools::fs::snapshot_into(
        &workspace,
        &runtime.config,
        (!command).then_some(&volumes[0]),
        &resources,
    )?;
    Ok(volumes)
}

// A selected host cache replaces only its nested path; a cloned view may hold an earlier private copy there, which capture excludes.
fn prepare_cache_links(
    root: &Path,
    cache: &cap_std::fs::Dir,
    grants: &[RuntimeGrant],
    cloned: bool,
) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;

    for grant in grants.iter().filter(|grant| grant.kind.is_cache()) {
        crate::tools::fs::reject_overlap(&grant.path, root)?;
        let relative = grant
            .kind
            .sandbox_cache_path()
            .ok_or(ToolError::Configuration)?;
        let parent = Path::new(relative).parent().ok_or(ToolError::Path)?;
        let mut current = cache.try_clone().map_err(|_| ToolError::Path)?;
        for component in parent.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(ToolError::Path);
            };
            if current.open_dir(name).is_err() {
                let mut builder = cap_std::fs::DirBuilder::new();
                cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
                current
                    .create_dir_with(name, &builder)
                    .map_err(|_| ToolError::Operation)?;
            }
            current = current
                .open_dir_nofollow(name)
                .map_err(|_| ToolError::Path)?;
        }
        let leaf = Path::new(relative).file_name().ok_or(ToolError::Path)?;
        match current.symlink_metadata(leaf) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(metadata) if cloned && metadata.is_dir() => current
                .remove_dir_all(leaf)
                .map_err(|_| ToolError::Operation)?,
            Ok(_) if cloned => current
                .remove_file(leaf)
                .map_err(|_| ToolError::Operation)?,
            _ => return Err(ToolError::ChangedInput),
        }
        rustix::fs::symlinkat(&grant.path, &current, leaf).map_err(|_| ToolError::Operation)?;
    }
    Ok(())
}

// A payload cannot mount, so any mount or foreign-device entry under the root is unexpected.
pub(super) fn storage_mounted(root: &Path) -> Result<bool, ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES {
        match directory.symlink_metadata(name) {
            Ok(metadata) if !metadata.is_dir() || metadata.dev() != parent.0 => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ToolError::Path),
        }
    }
    Ok(super::ffi::mounts()?
        .iter()
        .any(|(_, target)| target.starts_with(root)))
}

#[derive(Debug)]
pub(super) struct CleanupFailed {
    pub(super) error: ToolError,
    pub(super) check: GuardCheck,
}

impl From<CleanupFailed> for ToolError {
    fn from(failed: CleanupFailed) -> Self {
        failed.error
    }
}

// Storage holds no OS resource beyond its files; the root's removal deletes them once nothing is mounted beneath it.
pub(super) fn cleanup_storage(root: &Path) -> Result<(), CleanupFailed> {
    match storage_mounted(root) {
        Ok(false) => Ok(()),
        Ok(true) => Err(CleanupFailed {
            error: ToolError::ProtectionUnavailable,
            check: GuardCheck::MountStillPresent,
        }),
        Err(error) => Err(CleanupFailed {
            error,
            check: GuardCheck::Inspection,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_disk_refusal_names_the_free_space_and_the_reserve() {
        assert!(reserve_refusal(GUARD_DISK_RESERVE_BYTES).is_ok());
        let Err(ToolError::DiskSpace(message)) = reserve_refusal(BYTES_PER_MEBIBYTE * 1500) else {
            panic!("a volume below the reserve must refuse");
        };
        assert!(message.contains("1500 MiB free on the disk holding /private/tmp"));
        assert!(message.contains(&format!(
            "keeps {} MiB free",
            GUARD_DISK_RESERVE_BYTES / BYTES_PER_MEBIBYTE
        )));
    }
}

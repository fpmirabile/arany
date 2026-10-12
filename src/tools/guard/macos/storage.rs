//! Fixed-size private disk images that hold a command unit's Workspace snapshot and scratch.

use super::super::CAPTURE_BYTES;
use super::system_program;
use crate::private_fs::open_directory;
use crate::tools::{ToolError, ToolRuntime};
use cap_std::fs::MetadataExt as _;
use nix::libc;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub(super) const STORAGE_BYTES_CAP: u64 = crate::tools::types::SCRATCH_BYTES;
const BYTES_PER_MEBIBYTE: u64 = 1024 * 1024;
const _: () = assert!(STORAGE_BYTES_CAP.is_multiple_of(BYTES_PER_MEBIBYTE));
const STORAGE_NAMES: [&str; 2] = ["workspace", "scratch"];
pub(super) const STORAGE_PENDING: &str = "storage.pending";

async fn disk_command(root: &Path, arguments: &[&str]) -> Result<(), ToolError> {
    if !system_program("/usr/bin/hdiutil")? {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut command = Command::new("/usr/bin/hdiutil");
    command
        .args(arguments)
        .env_clear()
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (status, _, _) = super::super::capture(
        command
            .spawn()
            .map_err(|_| ToolError::ProtectionUnavailable)?,
        CAPTURE_BYTES,
        Duration::from_secs(20),
    )
    .await?;
    if status.success() {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) async fn prepare_storage(root: &Path) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    use cap_fs_ext::{DirExt, OpenOptionsFollowExt};
    use cap_std::fs::{OpenOptionsExt, PermissionsExt};

    open_directory(root, false).map_err(|_| ToolError::Path)?;
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    let mut volumes = Vec::with_capacity(2);
    for name in STORAGE_NAMES {
        let image_name = format!("{name}.dmg");
        for entry in [name, image_name.as_str()] {
            match directory.symlink_metadata(entry) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(ToolError::ChangedInput),
            }
        }
        let mut builder = cap_std::fs::DirBuilder::new();
        cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
        directory
            .create_dir_with(name, &builder)
            .map_err(|_| ToolError::Operation)?;
        let image = root.join(&image_name);
        let mount = root.join(name);
        let image_path = image.to_str().ok_or(ToolError::Path)?;
        let mount_path = mount.to_str().ok_or(ToolError::Path)?;
        let size = format!("{}m", STORAGE_BYTES_CAP / BYTES_PER_MEBIBYTE);
        disk_command(
            root,
            &[
                "create",
                "-quiet",
                "-size",
                &size,
                "-layout",
                "NONE",
                "-type",
                "UDIF",
                "-fs",
                "Case-sensitive HFS+",
                "-nospotlight",
                "-volname",
                name,
                image_path,
            ],
        )
        .await?;
        let image = directory
            .open_with(
                &image_name,
                cap_std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .follow(cap_fs_ext::FollowSymlinks::No),
            )
            .map_err(|_| ToolError::Path)?;
        let metadata = image.metadata().map_err(|_| ToolError::Path)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
            || metadata.len() != STORAGE_BYTES_CAP
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        image
            .set_permissions(cap_std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ToolError::Path)?;
        directory
            .open_with(
                STORAGE_PENDING,
                cap_std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .follow(cap_fs_ext::FollowSymlinks::No),
            )
            .map_err(|_| ToolError::Operation)?;
        disk_command(
            root,
            &[
                "attach",
                "-quiet",
                "-kernel",
                image_path,
                "-mountpoint",
                mount_path,
                "-mount",
                "required",
                "-owners",
                "on",
                "-nobrowse",
                "-noautoopen",
                "-noautofsck",
                "-verify",
            ],
        )
        .await?;
        directory
            .remove_file(STORAGE_PENDING)
            .map_err(|_| ToolError::Operation)?;
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
        volume
            .set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ToolError::Path)?;
        volumes.push(volume);
    }
    volumes
        .try_into()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

fn verify_volume(volume: &cap_std::fs::Dir, parent_device: u64) -> Result<(), ToolError> {
    let metadata = volume.dir_metadata().map_err(|_| ToolError::Path)?;
    let capacity = rustix::fs::fstatvfs(volume).map_err(|_| ToolError::ProtectionUnavailable)?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.dev() == parent_device
        || capacity.f_blocks.checked_mul(capacity.f_frsize) != Some(STORAGE_BYTES_CAP)
        || !capacity
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::NOSUID)
        || capacity
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::RDONLY)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(super) fn verify_storage(root: &Path) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;

    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES {
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
        if volume.dir_metadata().map_err(|_| ToolError::Path)?.mode() & 0o7777 != 0o700 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

pub(super) async fn prepare_command_storage(
    root: &Path,
    runtime: &ToolRuntime,
) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    use cap_fs_ext::DirExt;

    runtime.config.validate()?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    if crate::tools::fs::identity(&workspace)? != runtime.workspace_identity {
        return Err(ToolError::ChangedInput);
    }
    crate::tools::fs::reject_overlap(root, &runtime.workspace)?;
    let volumes = prepare_storage(root).await?;
    let directory = crate::tools::fs::open_directory(root)?;
    let mut builder = cap_std::fs::DirBuilder::new();
    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
    directory
        .create_dir_with("runtime", &builder)
        .map_err(|_| ToolError::Operation)?;
    let resources = directory
        .open_dir_nofollow("runtime")
        .map_err(|_| ToolError::Path)?;
    crate::tools::fs::snapshot_into(&workspace, &runtime.config, Some(&volumes[0]), &resources)?;
    Ok(volumes)
}

pub(super) fn storage_mounted(root: &Path) -> Result<bool, ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES.into_iter().chain([STORAGE_PENDING]) {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::Path),
        };
        if name == STORAGE_PENDING || !metadata.is_dir() || metadata.dev() != parent.0 {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn cleanup_storage(root: &Path) -> Result<(), ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::Path),
        };
        if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(ToolError::Path);
        }
        if metadata.dev() != parent.0 {
            let mount = root.join(name);
            disk_command(
                root,
                &[
                    "detach",
                    "-force",
                    "-quiet",
                    mount.to_str().ok_or(ToolError::Path)?,
                ],
            )
            .await?;
        }
    }
    if storage_mounted(root)? {
        Err(ToolError::ProtectionUnavailable)
    } else {
        Ok(())
    }
}

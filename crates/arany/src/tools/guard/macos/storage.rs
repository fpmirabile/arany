use super::super::{GUARD_CAPTURE_BYTES, GuardCheck, TEARDOWN_WAIT, retry_transient};
use super::system_program;
use crate::private_fs::open_directory;
use crate::tools::toolchains::RuntimeGrant;
use crate::tools::{ToolError, ToolRuntime};
use crate::validate::MAX_PATH_BYTES;
use cap_std::fs::MetadataExt as _;
use core_foundation::{base::CFType, dictionary::CFDictionary, string::CFString};
use nix::libc;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub(super) const STORAGE_BYTES_CAP: u64 = crate::tools::types::SCRATCH_BYTES;
const BYTES_PER_MEBIBYTE: u64 = 1024 * 1024;
const _: () = assert!(STORAGE_BYTES_CAP.is_multiple_of(BYTES_PER_MEBIBYTE));
const STORAGE_NAMES: [&str; 3] = ["workspace", "scratch", "cache"];
pub(super) const GIT_VOLUME: &str = "scratch";
#[cfg(any(test, feature = "test-support"))]
pub(super) const FILL_CHUNK_BYTES: usize = 1 << 20;
const CLEANUP_ATTEMPTS: usize = 8;
const CLEANUP_PAUSE: Duration = Duration::from_millis(250);
pub(super) const STORAGE_PENDING: &str = "storage.pending";

pub(super) use super::super::admission::{read_record, write_record};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingAttachment {
    pub(super) version: u32,
    pub(super) image: String,
    pub(super) boot: uuid::Uuid,
    pub(super) state: AttachmentState,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum AttachmentState {
    Attaching,
    Observed,
}

async fn disk_command(root: &Path, arguments: &[&str]) -> Result<Vec<u8>, ToolError> {
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
    let (status, stdout, _) = super::super::capture(
        command
            .spawn()
            .map_err(|_| ToolError::ProtectionUnavailable)?,
        GUARD_CAPTURE_BYTES,
        Duration::from_secs(20),
    )
    .await?;
    if status.success() {
        Ok(stdout)
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

#[cfg(test)]
pub(super) async fn prepare_storage(root: &Path) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    let volumes = prepare_storage_with_sizes(
        root,
        &[
            ("workspace", STORAGE_BYTES_CAP),
            ("scratch", STORAGE_BYTES_CAP),
        ],
    )
    .await?;
    volumes
        .try_into()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

async fn prepare_storage_with_sizes(
    root: &Path,
    sizes: &[(&str, u64)],
) -> Result<Vec<cap_std::fs::Dir>, ToolError> {
    use cap_fs_ext::{DirExt, OpenOptionsFollowExt};
    use cap_std::fs::{OpenOptionsExt, PermissionsExt};

    if sizes.is_empty()
        || sizes.len() > STORAGE_NAMES.len()
        || sizes.iter().any(|(name, bytes)| {
            !STORAGE_NAMES.contains(name)
                || *bytes == 0
                || !bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
                || *bytes > crate::tools::types::MAX_BUILD_CACHE_BYTES
        })
    {
        return Err(ToolError::Configuration);
    }
    open_directory(root, false).map_err(|_| ToolError::Path)?;
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    let mut volumes = Vec::with_capacity(sizes.len());
    for (name, bytes) in sizes {
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
        let size = format!("{}m", bytes / BYTES_PER_MEBIBYTE);
        let volume_name = format!(
            "{}-{name}",
            root.file_name()
                .and_then(|name| name.to_str())
                .ok_or(ToolError::Path)?
        );
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
                &volume_name,
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
            || metadata.len() != *bytes
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        image
            .set_permissions(cap_std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ToolError::Path)?;
        write_record(
            &directory,
            &format!("{name}.image.json"),
            &ImageIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                bytes: *bytes,
            },
        )?;
        write_record(
            &directory,
            STORAGE_PENDING,
            &PendingAttachment {
                version: 1,
                image: (*name).to_owned(),
                boot: super::ffi::boot_session()?,
                state: AttachmentState::Attaching,
            },
        )?;
        // Kernel-only attachment: killing a userspace disk-image helper before unmount triggers an improper-eject warning.
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
        verify_volume(&volume, parent.0, *bytes)?;
        volume
            .set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ToolError::Path)?;
        volumes.push(volume);
    }
    Ok(volumes)
}

fn verify_volume(
    volume: &cap_std::fs::Dir,
    parent_device: u64,
    expected_bytes: u64,
) -> Result<(), ToolError> {
    let metadata = volume.dir_metadata().map_err(|_| ToolError::Path)?;
    let capacity = rustix::fs::fstatvfs(volume).map_err(|_| ToolError::ProtectionUnavailable)?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.dev() == parent_device
        || capacity.f_blocks.checked_mul(capacity.f_frsize) != Some(expected_bytes)
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

fn command_volume_sizes(limits: &crate::tools::types::ToolLimits) -> Vec<(&'static str, u64)> {
    let mut sizes = vec![
        ("workspace", limits.project_bytes),
        ("scratch", limits.scratch_bytes),
    ];
    if limits.cache_bytes > 0 {
        sizes.push(("cache", limits.cache_bytes));
    }
    sizes
}

pub(super) fn verify_storage(
    root: &Path,
    limits: &crate::tools::types::ToolLimits,
) -> Result<(), ToolError> {
    verify_volumes(root, &command_volume_sizes(limits))
}

pub(super) fn verify_git_storage(root: &Path) -> Result<(), ToolError> {
    verify_volumes(root, &git_volume_sizes())
}

fn git_volume_sizes() -> [(&'static str, u64); 1] {
    [(GIT_VOLUME, STORAGE_BYTES_CAP)]
}

fn verify_volumes(root: &Path, sizes: &[(&str, u64)]) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;

    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for (name, bytes) in sizes {
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0, *bytes)?;
        if volume.dir_metadata().map_err(|_| ToolError::Path)?.mode() & 0o7777 != 0o700 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

pub(super) async fn prepare_git_storage(root: &Path) -> Result<Vec<cap_std::fs::Dir>, ToolError> {
    prepare_storage_with_sizes(root, &git_volume_sizes()).await
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn exhaust_git_storage(volume: &cap_std::fs::Dir) -> std::io::Result<()> {
    use std::io::{Error, Write as _};

    let capacity = rustix::fs::fstatvfs(volume)?;
    let block_bytes = usize::try_from(capacity.f_frsize)
        .ok()
        .filter(|bytes| (1..=FILL_CHUNK_BYTES).contains(bytes))
        .ok_or_else(|| Error::other("invalid Git volume allocation size"))?;
    let mut file = volume.create("fill")?;
    let chunk = vec![0_u8; FILL_CHUNK_BYTES];
    // A failed bulk allocation can leave room for the much smaller Git view.
    for bytes in [chunk.len(), block_bytes] {
        let mut full = false;
        for _ in 0..=STORAGE_BYTES_CAP / bytes as u64 {
            match file
                .write_all(&chunk[..bytes])
                .and_then(|()| file.sync_all())
            {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
                    full = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !full {
            return Err(Error::other(
                "Git volume did not reach ENOSPC within its bound",
            ));
        }
    }
    drop(file);
    if rustix::fs::fstatvfs(volume)?.f_bavail != 0 {
        return Err(Error::other(
            "Git volume still has available blocks after close",
        ));
    }
    match volume
        .create("full-probe")
        .and_then(|mut file| file.write_all(b"full").and_then(|()| file.sync_all()))
    {
        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => Ok(()),
        Err(error) => Err(error),
        Ok(()) => Err(Error::other("Git volume still permits a new file write")),
    }
}

pub(super) async fn prepare_command_storage(
    root: &Path,
    runtime: &ToolRuntime,
    limits: &crate::tools::types::ToolLimits,
    command: bool,
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
    let sizes = command_volume_sizes(limits);
    let volumes = prepare_storage_with_sizes(root, &sizes).await?;
    let (cache_name, cache_volume) = if limits.cache_bytes > 0 {
        ("cache", volumes.get(2).ok_or(ToolError::Operation)?)
    } else {
        ("scratch", volumes.get(1).ok_or(ToolError::Operation)?)
    };
    prepare_cache_links(
        root,
        cache_name,
        cache_volume,
        &runtime.config.runtime_grants,
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

fn prepare_cache_links(
    root: &Path,
    volume_name: &str,
    cache: &cap_std::fs::Dir,
    grants: &[RuntimeGrant],
) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;

    let cache_path = root.join(volume_name);
    for grant in grants.iter().filter(|grant| grant.kind.is_cache()) {
        crate::tools::fs::reject_overlap(&grant.path, root)?;
        let relative = grant
            .kind
            .sandbox_cache_path()
            .ok_or(ToolError::Configuration)?;
        let destination = cache_path.join(relative);
        if std::fs::symlink_metadata(&destination).is_ok() {
            return Err(ToolError::ChangedInput);
        }
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
        rustix::fs::symlinkat(&grant.path, &current, leaf).map_err(|_| ToolError::Operation)?;
    }
    Ok(())
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

// The cleanup future can run inside the spawned, Send Run task, so the check is shared through a Mutex.
struct CheckCell(std::sync::Mutex<GuardCheck>);

impl CheckCell {
    fn new(check: GuardCheck) -> Self {
        Self(std::sync::Mutex::new(check))
    }

    fn set(&self, check: GuardCheck) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = check;
    }

    fn get(&self) -> GuardCheck {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(super) async fn cleanup_storage(root: &Path) -> Result<(), CleanupFailed> {
    cleanup_storage_until(root, tokio::time::Instant::now() + TEARDOWN_WAIT).await
}

pub(super) async fn cleanup_storage_until(
    root: &Path,
    deadline: tokio::time::Instant,
) -> Result<(), CleanupFailed> {
    let check = CheckCell::new(GuardCheck::Inspection);
    retry_transient(CLEANUP_ATTEMPTS, CLEANUP_PAUSE, deadline, || {
        cleanup_storage_once(root, &check)
    })
    .await
    .map_err(|error| CleanupFailed {
        error,
        check: check.get(),
    })
}

async fn cleanup_storage_once(root: &Path, check: &CheckCell) -> Result<(), ToolError> {
    check.set(GuardCheck::Inspection);
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    let mut has_images = false;
    for name in STORAGE_NAMES
        .into_iter()
        .flat_map(|name| [format!("{name}.dmg"), format!("{name}.image.json")])
        .chain([STORAGE_PENDING.to_owned()])
    {
        match directory.symlink_metadata(name) {
            Ok(_) => has_images = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ToolError::Path),
        }
    }
    if !has_images {
        check.set(GuardCheck::MountStillPresent);
        return if !storage_mounted(root)?
            && !super::ffi::mounts()?
                .iter()
                .any(|(_, target)| target.starts_with(root))
        {
            Ok(())
        } else {
            Err(ToolError::ProtectionUnavailable)
        };
    }
    let mut devices = Vec::new();
    let mut pending: Option<PendingAttachment> = match directory.symlink_metadata(STORAGE_PENDING) {
        Ok(_) => Some(read_record(&directory, STORAGE_PENDING)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(ToolError::Path),
    };
    let mut admitted_image = false;
    check.set(GuardCheck::ListingUnavailable);
    let attached = disk_command(root, &["info", "-plist"]).await?;
    let owned_attachments = attachments(&attached, root)?;
    check.set(GuardCheck::Inspection);
    if let Some(pending) = &pending {
        if pending.version != 1 || !STORAGE_NAMES.contains(&pending.image.as_str()) {
            return Err(ToolError::ProtectionUnavailable);
        }
        if pending.boot.is_nil() {
            return Err(ToolError::ProtectionUnavailable);
        }
        verify_image(&directory, &pending.image)?;
        if pending.boot == super::ffi::boot_session()?
            && pending.state == AttachmentState::Attaching
            && !owned_attachments
                .iter()
                .any(|attachment| attachment.name == pending.image)
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    for name in STORAGE_NAMES {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::Path),
        };
        if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(ToolError::Path);
        }
        if directory
            .symlink_metadata(format!("{name}.image.json"))
            .is_ok()
        {
            verify_image(&directory, name)?;
            admitted_image = true;
        }
        let owned = owned_attachments
            .iter()
            .find(|attachment| attachment.name == name);
        if let Some(attachment) = owned {
            verify_image(&directory, name)?;
            check.set(GuardCheck::MountStillPresent);
            let mounts = super::ffi::mounts()?;
            for (device, target) in &mounts {
                if device == &attachment.device && target != &root.join(name) {
                    return Err(ToolError::ProtectionUnavailable);
                }
            }
            if metadata.dev() != parent.0 {
                if !mounts.iter().any(|(device, target)| {
                    device == &attachment.device && target == &root.join(name)
                }) {
                    return Err(ToolError::ProtectionUnavailable);
                }
            } else if mounts.iter().any(|(_, target)| target == &root.join(name)) {
                return Err(ToolError::ProtectionUnavailable);
            }
            check.set(GuardCheck::Inspection);
            devices.push(attachment.device.clone());
        } else if metadata.dev() != parent.0 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    check.set(GuardCheck::MountStillPresent);
    if super::ffi::mounts()?.iter().any(|(_, target)| {
        target.starts_with(root) && !STORAGE_NAMES.iter().any(|name| target == &root.join(name))
    }) {
        return Err(ToolError::ProtectionUnavailable);
    }
    if let Some(pending) = &mut pending
        && owned_attachments
            .iter()
            .any(|attachment| attachment.name == pending.image)
    {
        pending.state = AttachmentState::Observed;
        super::super::admission::replace_record(&directory, STORAGE_PENDING, pending)?;
    }
    for device in devices {
        // Recheck OS ownership immediately before acting on a potentially reused disk number.
        check.set(GuardCheck::ListingUnavailable);
        let current = disk_command(root, &["info", "-plist"]).await?;
        let current = attachments(&current, root)?;
        check.set(GuardCheck::DeviceUnlisted);
        super::super::require_listed(
            current.iter().map(|attachment| attachment.device.as_path()),
            device.as_path(),
        )?;
        check.set(GuardCheck::DetachFailed);
        disk_command(
            root,
            &[
                "detach",
                "-force",
                "-quiet",
                device.to_str().ok_or(ToolError::Path)?,
            ],
        )
        .await?;
    }
    if pending.is_some() && !admitted_image {
        return Err(ToolError::ProtectionUnavailable);
    }
    confirm_detached(root, &directory, pending.is_some(), check).await
}

async fn confirm_detached(
    root: &Path,
    directory: &cap_std::fs::Dir,
    pending: bool,
    check: &CheckCell,
) -> Result<(), ToolError> {
    check.set(GuardCheck::ListingUnavailable);
    let remaining = disk_command(root, &["info", "-plist"]).await?;
    let remaining = attachments(&remaining, root)?;
    check.set(GuardCheck::ImageStillListed);
    if !remaining.is_empty() {
        return Err(ToolError::ProtectionUnavailable);
    }
    if pending {
        check.set(GuardCheck::Inspection);
        directory
            .remove_file(STORAGE_PENDING)
            .map_err(|_| ToolError::Operation)?;
    }
    check.set(GuardCheck::MountStillPresent);
    if storage_mounted(root)? {
        Err(ToolError::ProtectionUnavailable)
    } else {
        Ok(())
    }
}

fn verify_image(directory: &cap_std::fs::Dir, name: &str) -> Result<(), ToolError> {
    use cap_fs_ext::OpenOptionsFollowExt;
    use cap_std::fs::OpenOptionsExt;
    let recorded: ImageIdentity = read_record(directory, &format!("{name}.image.json"))?;
    let image = directory
        .open_with(
            format!("{name}.dmg"),
            cap_std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .follow(cap_fs_ext::FollowSymlinks::No),
        )
        .map_err(|_| ToolError::Path)?;
    let metadata = image.metadata().map_err(|_| ToolError::Path)?;
    if recorded.bytes == 0
        || recorded.bytes > crate::tools::types::MAX_BUILD_CACHE_BYTES
        || !recorded.bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() != recorded.bytes
        || metadata.dev() != recorded.device
        || metadata.ino() != recorded.inode
        || crate::private_fs::check_acl(&image, true).is_err()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

struct Attachment {
    name: &'static str,
    device: std::path::PathBuf,
}

fn value(dictionary: &CFDictionary<CFString, CFType>, key: &str) -> Result<CFType, ToolError> {
    dictionary
        .find(CFString::new(key))
        .map(|value| value.clone())
        .ok_or(ToolError::ProtectionUnavailable)
}

fn attachments(bytes: &[u8], root: &Path) -> Result<Vec<Attachment>, ToolError> {
    let dictionary = super::ffi::dictionary(&super::ffi::property_list(bytes)?)?;
    let images = super::ffi::array(&value(&dictionary, "images")?)?;
    if images.len() > 1024 {
        return Err(ToolError::Limit);
    }
    let mut result = Vec::new();
    for image in images.iter() {
        let image = super::ffi::dictionary(&image)?;
        let image_path = value(&image, "image-path")?
            .downcast::<CFString>()
            .ok_or(ToolError::ProtectionUnavailable)?
            .to_string();
        if image_path.len() > MAX_PATH_BYTES {
            return Err(ToolError::Limit);
        }
        let Some(name) = STORAGE_NAMES
            .into_iter()
            .find(|name| root.join(format!("{name}.dmg")) == Path::new(&image_path))
        else {
            continue;
        };
        if result
            .iter()
            .any(|attachment: &Attachment| attachment.name == name)
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let entities = super::ffi::array(&value(&image, "system-entities")?)?;
        if entities.len() != 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
        let entry = entities.get(0).ok_or(ToolError::ProtectionUnavailable)?;
        let entity = super::ffi::dictionary(&entry)?;
        let device = value(&entity, "dev-entry")?
            .downcast::<CFString>()
            .ok_or(ToolError::ProtectionUnavailable)?
            .to_string();
        let number = device
            .strip_prefix("/dev/disk")
            .ok_or(ToolError::ProtectionUnavailable)?;
        if number.is_empty()
            || number.len() > 10
            || !number.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        if let Some(mount) = entity.find(CFString::new("mount-point")) {
            let mount = mount
                .downcast::<CFString>()
                .ok_or(ToolError::ProtectionUnavailable)?
                .to_string();
            if Path::new(&mount) != root.join(name) {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        result.push(Attachment {
            name,
            device: device.into(),
        });
    }
    Ok(result)
}

#[cfg(test)]
pub(super) async fn owned_devices(root: &Path) -> Result<Vec<std::path::PathBuf>, ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let bytes = disk_command(root, &["info", "-plist"]).await?;
    let attached = attachments(&bytes, root)?;
    for attachment in &attached {
        verify_image(&directory, attachment.name)?;
    }
    Ok(attached
        .into_iter()
        .map(|attachment| attachment.device)
        .collect())
}

#[cfg(test)]
pub(super) async fn detach_owned_image(root: &Path, name: &str) -> Result<(), ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    verify_image(&directory, name)?;
    let bytes = disk_command(root, &["info", "-plist"]).await?;
    let attached = attachments(&bytes, root)?;
    let Some(attachment) = attached.iter().find(|attachment| attachment.name == name) else {
        return Ok(());
    };
    if super::ffi::mounts()?
        .iter()
        .any(|(device, target)| device == &attachment.device && target != &root.join(name))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    disk_command(
        root,
        &[
            "detach",
            "-force",
            "-quiet",
            attachment.device.to_str().ok_or(ToolError::Path)?,
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_inventory_requires_exact_owned_device_and_mount() {
        let root = Path::new("/private/tmp/arany-inventory-fixture");
        for (case, image, device, mount, duplicate, accepted) in [
            (
                "owned",
                "workspace.dmg",
                "/dev/disk12",
                "workspace",
                false,
                true,
            ),
            ("foreign", "other.dmg", "/dev/disk12", "other", false, true),
            (
                "foreign mount",
                "workspace.dmg",
                "/dev/disk12",
                "other",
                false,
                false,
            ),
            (
                "partition",
                "workspace.dmg",
                "/dev/disk12s1",
                "workspace",
                false,
                false,
            ),
            (
                "invalid device",
                "workspace.dmg",
                "/dev/null",
                "workspace",
                false,
                false,
            ),
            (
                "duplicate",
                "workspace.dmg",
                "/dev/disk12",
                "workspace",
                true,
                false,
            ),
        ] {
            let image = format!(
                "<dict><key>image-path</key><string>{}/{image}</string><key>system-entities</key><array><dict><key>dev-entry</key><string>{device}</string><key>mount-point</key><string>{}/{mount}</string></dict></array></dict>",
                root.display(),
                root.display()
            );
            let xml = format!(
                "<plist version=\"1.0\"><dict><key>images</key><array>{image}{}</array></dict></plist>",
                if duplicate { image.as_str() } else { "" }
            );
            let result = attachments(xml.as_bytes(), root);
            assert_eq!(result.is_ok(), accepted, "inventory case {case}");
            if case == "foreign" {
                assert!(result.as_ref().unwrap().is_empty());
            }
            if case == "owned" {
                assert_eq!(result.unwrap()[0].device, Path::new(device));
            }
        }
        let pathless = "<plist version=\"1.0\"><dict><key>images</key><array><dict><key>system-entities</key><array/></dict></array></dict></plist>";
        assert!(attachments(pathless.as_bytes(), root).is_err());
        for bytes in [
            b"not a plist".as_slice(),
            b"<plist><dict/></plist>",
            b"<plist><array/></plist>",
        ] {
            assert!(attachments(bytes, root).is_err());
        }
    }
}

use super::*;
use rustix::mount::{MountFlags, MoveMountFlags, OpenTreeFlags};
use serde::{Deserialize, Serialize};

pub(super) const SETUP_INODES: usize = 16;

mod masks;
#[cfg(test)]
mod tests;

pub(in crate::tools::guard) fn selected(
    intent: &EffectIntent,
    config: &crate::tools::config::Config,
) -> bool {
    primary_command(intent) && config.workspace_paths == ["."]
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Grant {
    path: PathBuf,
    device: u64,
    inode: u64,
    digest: [u8; 32],
}

impl Grant {
    pub(super) fn new(runtime: &ToolRuntime, intent: &EffectIntent) -> Self {
        Self {
            path: runtime.workspace.clone(),
            device: intent.workspace_device,
            inode: intent.workspace_inode,
            digest: intent.digest(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools::guard) struct Attestation {
    digest: [u8; 32],
    namespaces: profile::Namespaces,
    masks: Vec<PathBuf>,
}

pub(super) struct View {
    root: PathBuf,
    grant: Grant,
    bytes: u64,
    inventory: masks::Inventory,
    attestation: Attestation,
}

fn unavailable<T>(result: Result<T, impl Sized>) -> Result<T, ToolError> {
    result.map_err(|_| ToolError::ProtectionUnavailable)
}

pub(super) fn no_submounts(mounts: &[MountInfoEntry], path: &Path) -> Result<(), ToolError> {
    if mounts
        .iter()
        .any(|entry| entry.path != path && entry.path.starts_with(path))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

impl View {
    pub(super) fn mount(root: &Path, grant: Grant, bytes: u64) -> Result<Self, ToolError> {
        if root.starts_with(&grant.path) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let deadline = std::time::Instant::now() + crate::tools::selection::PHASE_TIME;
        let source = crate::tools::fs::open_directory(&grant.path)?;
        if crate::tools::fs::identity(&source)? != (grant.device, grant.inode) {
            return Err(ToolError::ChangedInput);
        }
        let source_path = unavailable(std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&source)
        )))?;
        if source_path != grant.path {
            return Err(ToolError::ChangedInput);
        }
        no_submounts(&read_mountinfo()?, &source_path)?;
        for name in [
            "lower",
            "upper",
            "work",
            "merged",
            "empty-directory",
            "probe-lower",
            "probe-upper",
            "probe-work",
            "probe-merged",
        ] {
            unavailable(std::fs::create_dir(root.join(name)))?;
        }
        unavailable(std::fs::File::create_new(root.join("empty-file")))?;
        let clone = unavailable(rustix::mount::open_tree(
            &source,
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC
                | OpenTreeFlags::AT_EMPTY_PATH,
        ))?;
        unavailable(rustix::mount::move_mount(
            &clone,
            "",
            rustix::fs::CWD,
            root.join("lower"),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        ))?;
        unavailable(rustix::mount::mount_remount(
            root.join("lower"),
            MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
            "",
        ))?;
        let lower = crate::tools::fs::open_directory(&root.join("lower"))?;
        if crate::tools::fs::identity(&lower)? != (grant.device, grant.inode) {
            return Err(ToolError::ChangedInput);
        }
        verify_mount_entries(&read_mountinfo()?, &[root.join("lower")])?;
        overlay_probe(root)?;
        let inventory = masks::Inventory::read(&lower, deadline)?;
        mount_overlay(root, "")?;
        let masks = inventory.install(root)?;
        let view = Self {
            root: root.to_owned(),
            attestation: Attestation {
                digest: grant.digest,
                namespaces: profile::Namespaces::read()?,
                masks,
            },
            grant,
            bytes,
            inventory,
        };
        view.verify()?;
        Ok(view)
    }

    pub(super) fn merged(&self) -> PathBuf {
        self.root.join("merged")
    }

    pub(super) fn attestation(&self) -> &Attestation {
        &self.attestation
    }

    pub(super) fn verify(&self) -> Result<(), ToolError> {
        supervisor::verify_tmpfs(&self.root, self.bytes)?;
        let mounts = read_mountinfo()?;
        let lower = self.root.join("lower");
        verify_mount_entries(&mounts, std::slice::from_ref(&lower))?;
        let entry = mounts
            .iter()
            .find(|entry| entry.path == lower)
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !option(&entry.options, b"ro")
            || crate::tools::fs::identity(&crate::tools::fs::open_directory(&lower)?)?
                != (self.grant.device, self.grant.inode)
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let merged = self.merged();
        let overlay = mounts
            .iter()
            .find(|entry| entry.path == merged && entry.filesystem == b"overlay")
            .ok_or(ToolError::ProtectionUnavailable)?;
        for (name, path) in [
            ("lowerdir", &lower),
            ("upperdir", &self.root.join("upper")),
            ("workdir", &self.root.join("work")),
        ] {
            if !option(
                &overlay.super_options,
                format!("{name}={}", path.display()).as_bytes(),
            ) {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        if mounts.iter().any(|entry| {
            entry.path.starts_with(&self.root)
                && entry.path != self.root
                && entry.path != lower
                && entry.path != merged
                && !self
                    .attestation
                    .masks
                    .iter()
                    .any(|mask| merged.join(mask) == entry.path)
        }) {
            return Err(ToolError::ProtectionUnavailable);
        }
        self.inventory.verify(&lower)?;
        verify_overlay(&mounts, &self.merged(), &self.attestation.masks)?;
        Ok(())
    }

    pub(super) fn relay(&self, child: &mut impl Write) -> Result<(), ToolError> {
        let mut input = std::io::BufReader::new(std::io::stdin().lock());
        let task = super::super::read_task(&mut input)?;
        if task.intent.digest() != self.grant.digest
            || !selected(&task.intent, &task.config)
            || task.preparation
        {
            return Err(ToolError::Configuration);
        }
        child
            .write_all(&encode_task(&task)?)
            .map_err(|_| ToolError::Operation)?;
        if let Some(cache) = task.cache {
            let mut length = [0; 8];
            input
                .read_exact(&mut length)
                .map_err(|_| ToolError::Operation)?;
            let bytes = u64::from_le_bytes(length);
            if bytes > cache.view_bytes {
                return Err(ToolError::Limit);
            }
            child.write_all(&length).map_err(|_| ToolError::Operation)?;
            if std::io::copy(&mut input.by_ref().take(bytes), child)
                .map_err(|_| ToolError::Operation)?
                != bytes
            {
                return Err(ToolError::Operation);
            }
        }
        let mut go = [0; 3];
        input
            .read_exact(&mut go)
            .map_err(|_| ToolError::Operation)?;
        if go != *b"GO\n" {
            return Err(ToolError::Configuration);
        }
        self.verify()?;
        child.write_all(&go).map_err(|_| ToolError::Operation)
    }

    pub(super) fn finish(&self) -> Result<(), ToolError> {
        let empty = unavailable(std::fs::read_dir(self.root.join("upper")))?
            .next()
            .transpose()
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .is_none();
        let report = Completion {
            digest: self.grant.digest,
            namespaces: profile::Namespaces::read()?,
            empty,
        };
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, &report).map_err(|_| ToolError::Operation)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|_| ToolError::Operation)
    }
}

fn mount_overlay(root: &Path, prefix: &str) -> Result<(), ToolError> {
    let layer = |name: &str| root.join(format!("{prefix}{name}"));
    let options = format!(
        "lowerdir={},upperdir={},workdir={},userxattr,redirect_dir=nofollow,metacopy=off",
        layer("lower").display(),
        layer("upper").display(),
        layer("work").display()
    );
    if options.len() > MOUNTINFO_LINE_CAP
        || options
            .bytes()
            .any(|byte| matches!(byte, b'\\' | b' ' | b'\n'))
        || root.as_os_str().as_encoded_bytes().contains(&b',')
        || root.as_os_str().as_encoded_bytes().contains(&b':')
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let options = std::ffi::CString::new(options).map_err(|_| ToolError::ProtectionUnavailable)?;
    unavailable(rustix::mount::mount(
        "overlay",
        layer("merged"),
        "overlay",
        MountFlags::NOSUID | MountFlags::NODEV,
        options.as_c_str(),
    ))
}

fn overlay_probe(root: &Path) -> Result<(), ToolError> {
    let lower = root.join("probe-lower/file");
    unavailable(std::fs::write(&lower, b"lower"))?;
    mount_overlay(root, "probe-")?;
    let merged = root.join("probe-merged/file");
    unavailable(std::fs::write(&merged, b"edit"))?;
    if unavailable(std::fs::read(&merged))? != b"edit"
        || unavailable(std::fs::read(&lower))? != b"lower"
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    unavailable(std::fs::remove_file(&merged))?;
    if !matches!(std::fs::symlink_metadata(&merged), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    unavailable(std::fs::write(&merged, b"recreated"))?;
    let created = root.join("probe-merged/created");
    unavailable(std::fs::write(&created, b"created"))?;
    if unavailable(std::fs::read(&merged))? != b"recreated"
        || unavailable(std::fs::read(&created))? != b"created"
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    unavailable(rustix::mount::unmount(
        root.join("probe-merged"),
        rustix::mount::UnmountFlags::empty(),
    ))?;
    for name in ["probe-lower", "probe-upper", "probe-work", "probe-merged"] {
        unavailable(std::fs::remove_dir_all(root.join(name)))?;
    }
    Ok(())
}

fn option(options: &[u8], expected: &[u8]) -> bool {
    options
        .split(|byte| *byte == b',')
        .filter(|option| *option == expected)
        .count()
        == 1
}

fn verify_overlay(
    mounts: &[MountInfoEntry],
    target: &Path,
    masks: &[PathBuf],
) -> Result<(), ToolError> {
    if masks.iter().any(|path| path.as_os_str().is_empty()) {
        let entries: Vec<_> = mounts.iter().filter(|entry| entry.path == target).collect();
        if entries
            .iter()
            .filter(|entry| {
                entry.filesystem == b"tmpfs"
                    && entry.source == b"tmpfs"
                    && entry.root == Path::new("/empty-directory")
                    && option(&entry.options, b"ro")
                    && entry.optional_fields == 0
            })
            .count()
            != 1
            || entries.len() > 2
            || mounts
                .iter()
                .any(|entry| entry.path != target && entry.path.starts_with(target))
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        return Ok(());
    }
    let entry = mounts
        .iter()
        .find(|entry| entry.path == target)
        .ok_or(ToolError::ProtectionUnavailable)?;
    if mounts.iter().filter(|entry| entry.path == target).count() != 1
        || entry.filesystem != b"overlay"
        || entry.source != b"overlay"
        || entry.root != Path::new("/")
        || entry.optional_fields != 0
        || !option(&entry.super_options, b"userxattr")
        || !option(&entry.options, b"rw")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let expected: Vec<_> = masks.iter().map(|mask| target.join(mask)).collect();
    verify_mount_entries(mounts, &expected)?;
    for mask in &expected {
        let entry = mounts
            .iter()
            .find(|entry| &entry.path == mask)
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !option(&entry.options, b"ro")
            || entry.filesystem != b"tmpfs"
            || entry.source != b"tmpfs"
            || ![Path::new("/empty-file"), Path::new("/empty-directory")]
                .contains(&entry.root.as_path())
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    if mounts.iter().any(|entry| {
        entry.path != target && entry.path.starts_with(target) && !expected.contains(&entry.path)
    }) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(in crate::tools::guard) fn verify_helper(
    attestation: &Attestation,
    task: &Task,
) -> Result<(), ToolError> {
    if attestation.digest != task.intent.digest()
        || !attestation
            .namespaces
            .supervisor_isolated_from(&task.parent_namespaces)
        || !profile::Namespaces::read()?.isolated_from(&attestation.namespaces)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mounts = read_mountinfo()?;
    if mounts
        .iter()
        .filter(|entry| entry.path == Path::new("/workspace"))
        .count()
        != 1
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    verify_overlay(&mounts, Path::new("/workspace"), &attestation.masks)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    digest: [u8; 32],
    namespaces: profile::Namespaces,
    empty: bool,
}

pub(super) async fn complete(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    (intent, parent): (&EffectIntent, &profile::Namespaces),
    observation: &mut ToolObservation,
    candidate: &mut super::super::CacheCandidate,
) -> Result<(), ToolError> {
    crate::fault::guard_completion(intent.id)?;
    let report: Completion =
        serde_json::from_slice(&read_line_capped(reader, MAX_PATH_BYTES).await?)
            .map_err(|_| ToolError::Operation)?;
    if report.digest != intent.digest() || !report.namespaces.supervisor_isolated_from(parent) {
        return Err(ToolError::ProtectionUnavailable);
    }
    if !report.empty {
        if !matches!(
            observation.disposition,
            crate::tools::ToolDisposition::Limit | crate::tools::ToolDisposition::Cancelled
        ) {
            observation.disposition = crate::tools::ToolDisposition::Denied;
            observation.output = "Command overlay capture refused: the private upper is not empty; nothing was published".into();
            observation.spill = None;
            observation.enforcement_cause = None;
            observation.limit_cause = None;
        }
        if *candidate != super::super::CacheCandidate::Uncertain {
            *candidate = super::super::CacheCandidate::Rejected;
        }
    }
    Ok(())
}

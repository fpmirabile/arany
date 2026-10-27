use super::*;
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests;

const PARENT_BYTES: u64 = crate::tools::types::COMMAND_PATH_METADATA_BYTES as u64;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Root {
    pub(super) relative: String,
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) directory: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools::guard::linux) struct Grant {
    path: PathBuf,
    device: u64,
    inode: u64,
    digest: [u8; 32],
    command_deadline: [u8; 8],
    roots: Vec<Root>,
}

impl Grant {
    pub(in crate::tools::guard::linux) fn new(
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        deadline: tokio::time::Instant,
    ) -> Result<Self, ToolError> {
        let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
        let roots = runtime
            .config
            .workspace_paths
            .iter()
            .map(|relative| {
                if relative.split('/').count() > crate::tools::types::COMMAND_SNAPSHOT_DEPTH {
                    return Err(ToolError::Limit);
                }
                let file = crate::tools::fs::open_file(&workspace, relative)?;
                let metadata = file.metadata().map_err(|_| ToolError::ChangedInput)?;
                single_link(&metadata)?;
                if (!metadata.is_dir() && !metadata.is_file()) || metadata.mode() & 0o6000 != 0 {
                    return Err(ToolError::FilesystemDenied);
                }
                Ok(Root {
                    relative: if relative == "." {
                        String::new()
                    } else {
                        relative.clone()
                    },
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    directory: metadata.is_dir(),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            path: runtime.workspace.clone(),
            device: intent.workspace_device,
            inode: intent.workspace_inode,
            digest: intent.digest(),
            command_deadline: super::super::super::selection_phase::encode_deadline(deadline)?,
            roots,
        })
    }

    fn verify(&self) -> Result<(), ToolError> {
        let workspace = crate::tools::fs::open_directory(&self.path)?;
        if crate::tools::fs::identity(&workspace)? != (self.device, self.inode) {
            return Err(ToolError::ChangedInput);
        }
        for root in &self.roots {
            let file = crate::tools::fs::open_file(
                &workspace,
                if root.relative.is_empty() {
                    "."
                } else {
                    &root.relative
                },
            )?;
            let metadata = file.metadata().map_err(|_| ToolError::ChangedInput)?;
            single_link(&metadata)?;
            if (metadata.dev(), metadata.ino(), metadata.is_dir())
                != (root.device, root.inode, root.directory)
            {
                return Err(ToolError::ChangedInput);
            }
        }
        Ok(())
    }

    pub(in crate::tools::guard::linux) fn workspace_mount(&self, seed: &Path) -> (PathBuf, bool) {
        if self.roots[0].relative.is_empty() {
            (seed.join("namespace/grant-0/merged"), true)
        } else {
            (seed.join("namespace/parents"), false)
        }
    }

    pub(in crate::tools::guard::linux) fn arguments(&self, command: &mut Command, seed: &Path) {
        let namespace = seed.join("namespace");
        if self.roots[0].relative.is_empty() {
            return;
        }
        for (index, root) in self.roots.iter().enumerate() {
            command
                .arg(if root.directory {
                    "--bind"
                } else {
                    "--ro-bind"
                })
                .arg(namespace.join(format!(
                    "grant-{index}/{}",
                    if root.directory { "merged" } else { "file" }
                )))
                .arg(Path::new("/workspace").join(&root.relative));
        }
        command.args(["--remount-ro", "/workspace"]);
    }
}

pub(in crate::tools::guard::linux) struct View {
    root: PathBuf,
    grant: Grant,
    bytes: u64,
    selection_ms: u64,
    directories: Vec<DirectoryView>,
    attestation: Attestation,
}

impl View {
    pub(in crate::tools::guard::linux) fn mount(
        root: &Path,
        grant: Grant,
        bytes: u64,
    ) -> Result<Self, ToolError> {
        grant.verify()?;
        if root.starts_with(&grant.path)
            || grant.roots.is_empty()
            || grant.roots.len() > crate::tools::config::MAX_WORKSPACE_PATHS
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let started = std::time::Instant::now();
        let deadline =
            super::super::super::selection_phase::decode_deadline(grant.command_deadline)?;
        let mounts = read_mountinfo()?;
        for source in &grant.roots {
            no_submounts(&mounts, &grant.path.join(&source.relative))?;
        }
        if !grant.roots[0].relative.is_empty() {
            mount_parents(root, &grant.roots)?;
        }
        let mut budget = crate::tools::fs::CommandScanBudget::default();
        let mut selector = crate::tools::selection::Selector::new(0, deadline);
        let mut directories = Vec::new();
        let mut masks = Vec::new();
        let mut mask_bytes = masks::MASK_ARGUMENT_BYTES;
        for (index, source) in grant.roots.iter().enumerate() {
            let private = root.join(format!("grant-{index}"));
            unavailable(std::fs::create_dir(&private))?;
            if source.directory {
                let available = mask_allowance(&grant.roots[index + 1..], mask_bytes)?;
                let view = DirectoryView::mount(
                    &private,
                    (
                        &grant.path.join(&source.relative),
                        source.clone(),
                        available,
                    ),
                    &mut budget,
                    &mut selector,
                    deadline,
                )?;
                mask_bytes -= view.inventory.argument_bytes()?;
                masks.extend(
                    view.masks
                        .iter()
                        .map(|path| Path::new(&source.relative).join(path)),
                );
                directories.push(view);
            } else {
                mount_file(&private, source, &grant.path)?;
            }
        }
        if !masks::Inventory::aliases_safe(&directories, &grant.roots) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let attestation = Attestation {
            digest: grant.digest,
            namespaces: profile::Namespaces::read()?,
            masks,
            roots: grant.roots.clone(),
        };
        let view = Self {
            root: root.to_owned(),
            grant,
            bytes,
            selection_ms: u64::try_from(started.elapsed().as_millis())
                .map_err(|_| ToolError::Limit)?,
            directories,
            attestation,
        };
        view.verify()?;
        Ok(view)
    }

    pub(in crate::tools::guard::linux) fn attestation(&self) -> &Attestation {
        &self.attestation
    }

    fn verify(&self) -> Result<(), ToolError> {
        self.grant.verify()?;
        supervisor::verify_tmpfs(&self.root, self.bytes)?;
        for directory in &self.directories {
            directory.verify()?;
        }
        let mounts = read_mountinfo()?;
        for (index, source) in self.grant.roots.iter().enumerate() {
            if !source.directory {
                verify_file(
                    &mounts,
                    &self.root.join(format!("grant-{index}/file")),
                    source,
                )?;
            }
        }
        if !self.grant.roots[0].relative.is_empty() {
            verify_parents(&mounts, &self.root.join("parents"), &self.grant.roots)?;
        }
        self.verify_mount_scope(&mounts)
    }

    fn verify_mount_scope(&self, mounts: &[MountInfoEntry]) -> Result<(), ToolError> {
        if mounts.iter().any(|entry| {
            entry.path.starts_with(&self.root)
                && entry.path != self.root
                && (self.grant.roots[0].relative.is_empty()
                    || entry.path != self.root.join("parents"))
                && !self
                    .directories
                    .iter()
                    .any(|view| entry.path.starts_with(&view.root))
                && !self.grant.roots.iter().enumerate().any(|(index, source)| {
                    !source.directory && entry.path == self.root.join(format!("grant-{index}/file"))
                })
        }) {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(())
    }
    pub(in crate::tools::guard::linux) fn relay(
        &self,
        child: &mut impl Write,
    ) -> Result<(), ToolError> {
        let mut input = std::io::BufReader::new(std::io::stdin().lock());
        let task = super::super::super::read_task(&mut input)?;
        if task.intent.digest() != self.grant.digest || !selected(&task.intent, &task.config) {
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

    pub(in crate::tools::guard::linux) fn finish(&self) -> Result<(), ToolError> {
        let mut output = std::io::stdout().lock();
        output
            .write_all(b"CAPTURE_READY\n")
            .and_then(|()| output.flush())
            .map_err(|_| ToolError::Operation)?;
        drop(output);
        let mut input = std::io::stdin().lock();
        let mut acknowledgement = [0; 8];
        input
            .read_exact(&mut acknowledgement)
            .map_err(|_| ToolError::Operation)?;
        if acknowledgement != *b"CAPTURE\n" {
            return Err(ToolError::Configuration);
        }
        let mut frame = [0; 8];
        input
            .read_exact(&mut frame)
            .map_err(|_| ToolError::Operation)?;
        let deadline = super::super::super::selection_phase::decode_deadline(frame)?;
        let capture = (|| {
            self.grant.verify()?;
            let roots = self
                .directories
                .iter()
                .map(|view| {
                    Ok((
                        &view.inventory.baseline,
                        crate::tools::fs::open_directory(&view.root.join("lower"))?,
                        crate::tools::fs::open_directory(&view.root.join("upper"))?,
                        view.grant.relative.as_str(),
                    ))
                })
                .collect::<Result<Vec<_>, ToolError>>()?;
            crate::tools::fs::command::overlay::capture_roots(&roots, deadline, self.bytes).map(
                |(changeset, mut summary)| {
                    summary.elapsed_ms = Some(self.selection_ms);
                    (changeset, summary)
                },
            )
        })();
        let report = Completion {
            digest: self.grant.digest,
            namespaces: profile::Namespaces::read()?,
            capture: capture.map_err(|error| match error {
                ToolError::Limit => CaptureFailure::Limit,
                ToolError::Timeout(_) => CaptureFailure::Timeout,
                ToolError::FilesystemDenied => CaptureFailure::Filesystem,
                ToolError::Conflict | ToolError::ChangedInput => CaptureFailure::Conflict,
                _ => CaptureFailure::Rejected,
            }),
        };
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, &report).map_err(|_| ToolError::Operation)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|_| ToolError::Operation)
    }
}

fn mask_allowance(remaining: &[Root], bytes: usize) -> Result<usize, ToolError> {
    let reserved = remaining
        .iter()
        .filter(|root| root.directory)
        .try_fold(0, |total, root| {
            serde_json::to_vec(&[Path::new(&root.relative).join("")])
                .map(|encoded| total + encoded.len())
                .map_err(|_| ToolError::ProtectionUnavailable)
        })?;
    bytes
        .checked_sub(reserved)
        .ok_or(ToolError::ProtectionUnavailable)
}

fn parent_inodes(roots: &[Root]) -> u64 {
    (roots.len() * crate::tools::types::COMMAND_SNAPSHOT_DEPTH + 1) as u64
}

fn mount_parents(root: &Path, roots: &[Root]) -> Result<(), ToolError> {
    let parents = root.join("parents");
    unavailable(std::fs::create_dir(&parents))?;
    let options = std::ffi::CString::new(format!(
        "size={PARENT_BYTES},nr_inodes={}",
        parent_inodes(roots)
    ))
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    unavailable(rustix::mount::mount(
        "tmpfs",
        &parents,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        options.as_c_str(),
    ))?;
    for source in roots {
        if source.directory {
            unavailable(std::fs::create_dir_all(parents.join(&source.relative)))?;
        } else {
            let target = parents.join(&source.relative);
            unavailable(std::fs::create_dir_all(
                target.parent().ok_or(ToolError::Path)?,
            ))?;
            unavailable(std::fs::File::create_new(target))?;
        }
    }
    unavailable(rustix::mount::mount_remount(
        &parents,
        MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
        options.as_c_str(),
    ))
}

fn single_link(metadata: &std::fs::Metadata) -> Result<(), ToolError> {
    if metadata.is_file() && metadata.nlink() != 1 {
        return Err(ToolError::Path);
    }
    Ok(())
}

fn mount_file(root: &Path, source: &Root, workspace: &Path) -> Result<(), ToolError> {
    let workspace = crate::tools::fs::open_directory(workspace)?;
    let file = crate::tools::fs::open_file(&workspace, &source.relative)?;
    let metadata = file.metadata().map_err(|_| ToolError::ChangedInput)?;
    single_link(&metadata)?;
    if (metadata.dev(), metadata.ino()) != (source.device, source.inode) {
        return Err(ToolError::ChangedInput);
    }
    let pinned = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&file));
    let target = root.join("file");
    unavailable(std::fs::File::create_new(&target))?;
    unavailable(rustix::mount::mount_bind(&pinned, &target))?;
    unavailable(rustix::mount::mount_remount(
        &target,
        MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
        "",
    ))
}

fn verify_file(mounts: &[MountInfoEntry], target: &Path, source: &Root) -> Result<(), ToolError> {
    verify_mount_entries(mounts, &[target.to_owned()])?;
    let entry = mounts
        .iter()
        .find(|entry| entry.path == target)
        .ok_or(ToolError::ProtectionUnavailable)?;
    let metadata = std::fs::symlink_metadata(target).map_err(|_| ToolError::ChangedInput)?;
    single_link(&metadata)?;
    if ![b"ro".as_slice(), b"nosuid", b"nodev"]
        .iter()
        .all(|flag| option(&entry.options, flag))
        || !metadata.is_file()
        || (metadata.dev(), metadata.ino()) != (source.device, source.inode)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_parents(
    mounts: &[MountInfoEntry],
    target: &Path,
    roots: &[Root],
) -> Result<(), ToolError> {
    let entries: Vec<_> = mounts.iter().filter(|entry| entry.path == target).collect();
    if entries.len() != 1 {
        return Err(ToolError::ProtectionUnavailable);
    }
    let entry = entries[0];
    if entry.filesystem != b"tmpfs"
        || entry.source != b"tmpfs"
        || entry.optional_fields != 0
        || !option(&entry.options, b"ro")
        || entry.root != Path::new("/")
        || !option(
            &entry.super_options,
            format!("size={}k", PARENT_BYTES / 1024).as_bytes(),
        )
        || !option(
            &entry.super_options,
            format!("nr_inodes={}", parent_inodes(roots)).as_bytes(),
        )
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(super) fn verify_workspace(
    mounts: &[MountInfoEntry],
    attestation: &Attestation,
    paths: &[String],
    workspace: &Path,
) -> Result<(), ToolError> {
    let roots = &attestation.roots;
    if roots.len() != paths.len()
        || roots
            .iter()
            .zip(paths)
            .any(|(root, path)| root.relative != if path == "." { "" } else { path })
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    if paths != ["."] {
        verify_parents(mounts, workspace, roots)?;
    }
    let mut expected = vec![workspace.to_owned()];
    for (index, root) in roots.iter().enumerate() {
        let target = workspace.join(&root.relative);
        if root.directory {
            let masks = attestation
                .masks
                .iter()
                .filter_map(|path| path.strip_prefix(&root.relative).ok().map(Path::to_owned))
                .collect::<Vec<_>>();
            verify_overlay(
                mounts,
                &target,
                &masks,
                &Path::new("/").join(format!("grant-{index}")),
            )?;
            expected.extend(masks.iter().map(|mask| target.join(mask)));
        } else {
            verify_file(mounts, &target, root)?;
        }
        expected.push(target);
    }
    if mounts
        .iter()
        .any(|entry| entry.path.starts_with(workspace) && !expected.contains(&entry.path))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

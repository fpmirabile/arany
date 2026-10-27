use super::*;
use rustix::mount::{MountFlags, MoveMountFlags, OpenTreeFlags};
use serde::{Deserialize, Serialize};

pub(super) const SETUP_INODES: usize = 16;

mod masks;
pub(super) use masks::MASK_ARGUMENT_BYTES;
mod scope;
pub(super) use scope::{Grant, View};
pub(in crate::tools::guard) mod cache;
#[cfg(test)]
mod tests;

pub(in crate::tools::guard) fn selected(
    intent: &EffectIntent,
    _config: &crate::tools::config::Config,
) -> bool {
    primary_command(intent)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools::guard) struct Attestation {
    digest: [u8; 32],
    namespaces: profile::Namespaces,
    masks: Vec<PathBuf>,
    roots: Vec<scope::Root>,
    cache_bytes: Option<u64>,
}

struct DirectoryView {
    root: PathBuf,
    grant: scope::Root,
    inventory: masks::Inventory,
    masks: Vec<PathBuf>,
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

impl DirectoryView {
    fn mount(
        root: &Path,
        (source_path, grant, mask_bytes): (&Path, scope::Root, usize),
        budget: &mut crate::tools::fs::CommandScanBudget,
        selector: &mut crate::tools::selection::Selector,
        deadline: std::time::Instant,
    ) -> Result<Self, ToolError> {
        if root.starts_with(source_path) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let lower = clone_lower(root, source_path, &grant)?;
        overlay_probe(root)?;
        let inventory = masks::Inventory::read_shared(
            &lower,
            (&grant.relative, mask_bytes),
            budget,
            selector,
            deadline,
        )?;
        mount_overlay(root, "")?;
        let masks = inventory.install(root)?;
        let view = Self {
            root: root.to_owned(),
            masks,
            grant,
            inventory,
        };
        view.verify()?;
        Ok(view)
    }

    pub(super) fn merged(&self) -> PathBuf {
        self.root.join("merged")
    }

    pub(super) fn verify(&self) -> Result<(), ToolError> {
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
        verify_layers(&mounts, &self.root)?;
        if mounts.iter().any(|entry| {
            entry.path.starts_with(&self.root)
                && entry.path != lower
                && entry.path != merged
                && !self
                    .masks
                    .iter()
                    .any(|mask| merged.join(mask) == entry.path)
        }) {
            return Err(ToolError::ProtectionUnavailable);
        }
        self.inventory.verify(&lower)?;
        let mask_root = Path::new("/").join(self.root.file_name().ok_or(ToolError::Path)?);
        verify_overlay(&mounts, &self.merged(), &self.masks, &mask_root)?;
        Ok(())
    }
}

fn verify_layers(mounts: &[MountInfoEntry], root: &Path) -> Result<(), ToolError> {
    let entry = mounts
        .iter()
        .find(|entry| entry.path == root.join("merged") && entry.filesystem == b"overlay")
        .ok_or(ToolError::ProtectionUnavailable)?;
    for (option_name, name) in [
        ("lowerdir", "lower"),
        ("upperdir", "upper"),
        ("workdir", "work"),
    ] {
        if !option(
            &entry.super_options,
            format!("{option_name}={}", root.join(name).display()).as_bytes(),
        ) {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

fn clone_lower(
    root: &Path,
    source_path: &Path,
    grant: &scope::Root,
) -> Result<cap_std::fs::Dir, ToolError> {
    let source = crate::tools::fs::open_directory(source_path)?;
    if crate::tools::fs::identity(&source)? != (grant.device, grant.inode) {
        return Err(ToolError::ChangedInput);
    }
    let source_path_requested = source_path;
    let source_path = unavailable(std::fs::read_link(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&source)
    )))?;
    if source_path != source_path_requested {
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
    Ok(lower)
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
    mask_root: &Path,
) -> Result<(), ToolError> {
    if masks.iter().any(|path| path.as_os_str().is_empty()) {
        let entries: Vec<_> = mounts.iter().filter(|entry| entry.path == target).collect();
        if entries
            .iter()
            .filter(|entry| {
                entry.filesystem == b"tmpfs"
                    && entry.source == b"tmpfs"
                    && entry.root == mask_root.join("empty-directory")
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
            || !["empty-file", "empty-directory"]
                .iter()
                .any(|name| entry.root == mask_root.join(name))
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
    cache::verify_helper(attestation.cache_bytes, task)?;
    if attestation.digest != task.intent.digest()
        || !attestation
            .namespaces
            .supervisor_isolated_from(&task.parent_namespaces)
        || !profile::Namespaces::read()?.isolated_from(&attestation.namespaces)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    scope::verify_workspace(
        &read_mountinfo()?,
        attestation,
        &task.config.workspace_paths,
        Path::new("/workspace"),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    digest: [u8; 32],
    namespaces: profile::Namespaces,
    capture: Result<
        (
            crate::tools::CommandChangeset,
            crate::tools::types::CommandSelectionSummary,
        ),
        CaptureFailure,
    >,
}

#[derive(Serialize, Deserialize)]
enum CaptureFailure {
    Limit,
    Timeout,
    Filesystem,
    Rejected,
    Conflict,
}

const COMPLETION_BYTES: usize =
    crate::tools::types::MAX_CHANGESET_SERIALIZED_BYTES + MAX_PATH_BYTES;

pub(super) async fn capture(
    unit: &mut Unit,
    (writer, reader): (
        &mut (impl tokio::io::AsyncWrite + Unpin),
        &mut (impl tokio::io::AsyncBufRead + Unpin),
    ),
    (runtime, intent, parent, deadline): (
        &ToolRuntime,
        &EffectIntent,
        &profile::Namespaces,
        tokio::time::Instant,
    ),
    (observation, candidate): (&mut ToolObservation, &mut super::super::CacheCandidate),
) -> Result<(), ToolError> {
    let truncation = if runtime.hook_stdin.is_some() {
        crate::tools::process::Truncation::Refuse
    } else {
        crate::tools::process::Truncation::Allow
    };
    let report_deadline = capture_phase(unit, intent, writer, reader, deadline)
        .await?
        .ok_or(ToolError::Operation)?;
    tokio::time::timeout_at(
        report_deadline,
        complete(reader, (intent, parent), observation, candidate, truncation),
    )
    .await
    .map_err(|_| ToolError::Operation)?
}

pub(super) async fn complete(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    (intent, parent): (&EffectIntent, &profile::Namespaces),
    observation: &mut ToolObservation,
    candidate: &mut super::super::CacheCandidate,
    truncation: crate::tools::process::Truncation,
) -> Result<(), ToolError> {
    crate::fault::guard_completion(intent.id)?;
    let report: Completion =
        serde_json::from_slice(&read_line_capped(reader, COMPLETION_BYTES).await?)
            .map_err(|_| ToolError::Operation)?;
    if report.digest != intent.digest() || !report.namespaces.supervisor_isolated_from(parent) {
        return Err(ToolError::ProtectionUnavailable);
    }
    let nonzero_exit = observation.disposition == crate::tools::ToolDisposition::Failed
        && serde_json::from_str::<crate::tools::types::CommandResult>(&observation.output)
            .is_ok_and(|result| result.valid() && result.exit_code.is_some_and(|code| code != 0));
    if observation.disposition != crate::tools::ToolDisposition::Succeeded
        && (!nonzero_exit || report.capture.is_err())
    {
        if !report
            .capture
            .as_ref()
            .is_ok_and(|(changeset, _)| changeset.files.is_empty())
            && *candidate != super::super::CacheCandidate::Uncertain
        {
            *candidate = super::super::CacheCandidate::Rejected;
        }
        if observation.disposition == crate::tools::ToolDisposition::Uncertain {
            *candidate = super::super::CacheCandidate::Uncertain;
        }
        return Ok(());
    }
    let captured = report
        .capture
        .and_then(|(changeset, selection)| merge(observation, changeset, selection, truncation));
    match captured {
        Ok(output) => observation.output = output,
        Err(failure) => {
            observation.disposition = match failure {
                CaptureFailure::Limit | CaptureFailure::Timeout => {
                    crate::tools::ToolDisposition::Limit
                }
                CaptureFailure::Filesystem | CaptureFailure::Rejected => {
                    crate::tools::ToolDisposition::Denied
                }
                CaptureFailure::Conflict => crate::tools::ToolDisposition::Conflict,
            };
            observation.output = "Command overlay capture refused; nothing was published".into();
            observation.spill = None;
            observation.enforcement_cause = matches!(failure, CaptureFailure::Filesystem)
                .then_some(crate::tools::EnforcementCause::FilesystemDenied);
            observation.limit_cause = match failure {
                CaptureFailure::Limit => Some(crate::tools::LimitCause::Resource),
                CaptureFailure::Timeout => Some(crate::tools::LimitCause::Timeout),
                CaptureFailure::Filesystem
                | CaptureFailure::Rejected
                | CaptureFailure::Conflict => None,
            };
            if *candidate != super::super::CacheCandidate::Uncertain {
                *candidate = super::super::CacheCandidate::Rejected;
            }
        }
    }
    Ok(())
}

fn merge(
    observation: &ToolObservation,
    changeset: crate::tools::CommandChangeset,
    selection: crate::tools::types::CommandSelectionSummary,
    truncation: crate::tools::process::Truncation,
) -> Result<String, CaptureFailure> {
    if !changeset.valid() || !selection.valid() {
        return Err(CaptureFailure::Rejected);
    }
    let result: crate::tools::types::CommandResult =
        serde_json::from_str(&observation.output).map_err(|_| CaptureFailure::Rejected)?;
    let stdout = crate::tools::output::Window::from_recorded(&result.stdout, result.stdout_total)
        .ok_or(CaptureFailure::Rejected)?;
    let stderr = crate::tools::output::Window::from_recorded(&result.stderr, result.stderr_total)
        .ok_or(CaptureFailure::Rejected)?;
    crate::tools::process::fit_selected_command(
        result.exit_code,
        &stdout,
        &stderr,
        changeset,
        observation.intent.limits.result_bytes as usize,
        truncation,
        selection,
    )
    .map_err(|_| CaptureFailure::Limit)
}

use super::*;
use std::os::unix::fs::MetadataExt;

const CAPTURED_LEAF: &str = "entry";
const TEMPORARY_PREFIX: &str = ".arany-remove-";

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum Phase {
    BeforeClaim,
    Claimed,
}

pub(super) fn remove(
    root: &Dir,
    parent: &ChangeParent,
    expected: Option<&FileVersion>,
    control: &MutationControl,
) -> Result<(), ToolError> {
    control.check()?;
    let temporary = format!("{TEMPORARY_PREFIX}{}", uuid::Uuid::now_v7());
    let mut builder = DirBuilder::new();
    use cap_std::fs::DirBuilderExt;
    builder.mode(0o700);
    parent
        .directory
        .create_dir_with(&temporary, &builder)
        .map_err(|_| ToolError::Operation)?;
    let result = (|| {
        let private = Dir::from_std_file(open_file(&parent.directory, &temporary)?);
        check_change_directory(root, &private)?;
        claim(root, parent, &private, expected, control)
    })();
    let cleanup = parent.directory.remove_dir(&temporary);
    if result.is_ok() || matches!(result, Err(ToolError::Conflict | ToolError::Operation)) {
        cleanup.map_err(|_| ToolError::Uncertain)?;
        control.sync_parent(&parent.directory)?;
    }
    result
}

fn claim(
    root: &Dir,
    parent: &ChangeParent,
    private: &Dir,
    expected: Option<&FileVersion>,
    control: &MutationControl,
) -> Result<(), ToolError> {
    let checked = open_file(&parent.directory, &parent.leaf).map_err(|_| ToolError::Conflict)?;
    let before = checked.metadata().map_err(|_| ToolError::Conflict)?;
    if !before.is_file() {
        return Err(ToolError::Conflict);
    }
    verify_change_parent(root, parent)?;
    #[cfg(test)]
    if let Some(hook) = control.removal_hook {
        hook(&parent.directory, &parent.leaf, Phase::BeforeClaim);
    }
    control.commit()?;
    rustix::fs::renameat_with(
        &parent.directory,
        &parent.leaf,
        private,
        CAPTURED_LEAF,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            ToolError::Conflict
        } else {
            ToolError::Operation
        }
    })?;
    #[cfg(test)]
    if let Some(hook) = control.removal_hook {
        hook(&parent.directory, &parent.leaf, Phase::Claimed);
    }
    let verified = (|| {
        let captured = open_file(private, CAPTURED_LEAF)?;
        let now = captured.metadata().map_err(|_| ToolError::Conflict)?;
        if (before.dev(), before.ino()) != (now.dev(), now.ino()) {
            return Err(ToolError::Conflict);
        }
        check_change_file(private, CAPTURED_LEAF, expected)?;
        verify_change_parent(root, parent)
    })();
    if verified.is_err() {
        return restore(parent, private, control, ToolError::Conflict);
    }
    if private.remove_file(CAPTURED_LEAF).is_err() {
        return restore(parent, private, control, ToolError::Operation);
    }
    control.sync_parent(private)?;
    Ok(())
}

fn restore(
    parent: &ChangeParent,
    private: &Dir,
    control: &MutationControl,
    failure: ToolError,
) -> Result<(), ToolError> {
    rustix::fs::renameat_with(
        private,
        CAPTURED_LEAF,
        &parent.directory,
        &parent.leaf,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|_| ToolError::Uncertain)?;
    control.sync_parent(private)?;
    control.sync_parent(&parent.directory)?;
    Err(failure)
}

#[cfg(test)]
mod tests;

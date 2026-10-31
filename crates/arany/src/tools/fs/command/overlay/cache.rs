use super::*;
use crate::store::{CACHE_ROOTS, CacheChange};
use crate::tools::fs::tree::{self, Pair};

#[cfg(test)]
mod tests;

// Streams the upper against the lower it covers; whiteouts, opaque directories and type changes remove
// the lower subtree beneath them.
pub(in crate::tools) fn capture(
    lower: &Dir,
    upper: &Dir,
    exclusions: &[String],
    deadline: Instant,
    limit: u64,
) -> Result<Vec<CacheChange>, ToolError> {
    let mut budget = CommandScanBudget::default();
    drop(readable(lower, "")?);
    drop(readable(upper, "")?);
    if metadata::validate(&pin(upper, "")?, &mut budget)? {
        return Err(ToolError::Path);
    }
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    let mut removing: Option<String> = None;
    let mut held = 0usize;
    let mut total = 0u64;
    tree::diff(
        &mut tree::Walk::new(lower, deadline)?,
        &mut tree::Walk::new(upper, deadline)?,
        |pair| {
            let (old, new) = match pair {
                Pair::Left(old) => (Some(old), None),
                Pair::Right(new) => (None, Some(new)),
                Pair::Both(old, new) => (Some(old), Some(new)),
            };
            let path = old.as_ref().or(new.as_ref()).map(|node| node.path.clone());
            let path = path.ok_or(ToolError::Operation)?;
            if removing
                .as_ref()
                .is_some_and(|root| !Path::new(&path).starts_with(root))
            {
                removing = None;
            }
            if path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
                return Err(ToolError::Limit);
            }
            if if selected(&path) {
                crate::store::validate_cache_path(&path).is_err()
            } else {
                !valid_relative(&path, false)
            } {
                return Err(ToolError::Path);
            }
            let old = old.map(|node| node.metadata);
            let new = new.map(|node| node.metadata);
            if let Some(old) = &old {
                admit(lower, &path, old, false)?;
            }
            let (mut opaque, mut whiteout) = (false, false);
            if let Some(new) = &new {
                whiteout = admit(upper, &path, new, true)?;
                opaque = metadata::validate(&pin(upper, &path)?, &mut budget)?;
            }
            if excluded(&path, exclusions) {
                return Ok(true);
            }
            if removing.is_none()
                && new.as_ref().is_some_and(|new| {
                    opaque
                        || whiteout
                        || old.as_ref().is_some_and(|old| old.is_dir() != new.is_dir())
                })
            {
                removing = Some(path.clone());
            }
            let removed = old.is_some()
                && removing.is_some()
                && selected(&path)
                && !exclusions
                    .iter()
                    .any(|root| Path::new(root).starts_with(&path));
            let mut charge = |path: &str| {
                held = held.checked_add(path.len()).ok_or(ToolError::Limit)?;
                if held > tree::HELD_NAME_BYTES {
                    return Err(ToolError::Limit);
                }
                Ok(())
            };
            if removed {
                charge(&path)?;
                removals.push(CacheChange::Remove(path.clone()));
            }
            if !selected(&path) {
                return Ok(true);
            }
            match (&old, &new) {
                (_, Some(new)) if new.is_file() => {
                    total = total.checked_add(new.len()).ok_or(ToolError::Limit)?;
                    charge(&path)?;
                    additions.push(CacheChange::File(path));
                }
                (_, Some(new)) if new.is_dir() => {
                    charge(&path)?;
                    additions.push(CacheChange::Directory(path));
                }
                (Some(old), None) if old.is_file() && !removed => {
                    total = total.checked_add(old.len()).ok_or(ToolError::Limit)?;
                }
                _ => {}
            }
            Ok(true)
        },
    )?;
    if total > limit {
        return Err(ToolError::Limit);
    }
    removals.reverse();
    removals.extend(additions);
    Ok(removals)
}

// Only directories, regular files and upper 0/0 whiteouts are admitted; returns whether it is a whiteout.
fn admit(
    root: &Dir,
    path: &str,
    entry: &cap_std::fs::Metadata,
    upper: bool,
) -> Result<bool, ToolError> {
    use cap_std::fs::FileTypeExt as _;
    let whiteout = upper && entry.file_type().is_char_device() && entry.rdev() == 0;
    if entry.mode() & 0o6000 != 0 || !(entry.is_dir() || entry.is_file() || whiteout) {
        return Err(ToolError::Path);
    }
    if !whiteout {
        drop(readable(root, path)?);
    }
    Ok(whiteout)
}

fn selected(path: &str) -> bool {
    CACHE_ROOTS
        .iter()
        .any(|root| Path::new(path).starts_with(root))
}

fn excluded(path: &str, exclusions: &[String]) -> bool {
    exclusions
        .iter()
        .any(|root| Path::new(path).starts_with(root))
}

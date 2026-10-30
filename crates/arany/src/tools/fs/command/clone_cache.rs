use super::*;
use crate::store::{CACHE_ROOTS, CacheChange};
use crate::tools::fs::tree::{self, Pair};
use cap_std::fs::{Metadata, MetadataExt};

#[cfg(test)]
mod tests;

// Diffs a quiescent copy-on-write cache view against the Store tree it was cloned from.
pub(in crate::tools) fn capture(
    lower: &Dir,
    view: &Dir,
    exclusions: &[String],
    deadline: Instant,
    limit: u64,
) -> Result<Vec<CacheChange>, ToolError> {
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    let mut held = 0usize;
    let mut total = 0u64;
    tree::diff(
        &mut tree::Walk::new(lower, deadline)?,
        &mut tree::Walk::new(view, deadline)?,
        |pair| {
            let path = match &pair {
                Pair::Left(node) | Pair::Right(node) | Pair::Both(_, node) => node.path.clone(),
            };
            if excluded(&path, exclusions) {
                return Ok(false);
            }
            let (old, new) = match pair {
                Pair::Left(old) => (Some(old.metadata), None),
                Pair::Right(new) => (None, Some(new.metadata)),
                Pair::Both(old, new) => (Some(old.metadata), Some(new.metadata)),
            };
            for metadata in old.iter().chain(&new) {
                validate(&path, metadata)?;
            }
            if !selected(&path) {
                return Ok(true);
            }
            held = held.checked_add(path.len()).ok_or(ToolError::Limit)?;
            if held > tree::HELD_NAME_BYTES {
                return Err(ToolError::Limit);
            }
            if old
                .as_ref()
                .is_some_and(|old| new.as_ref().is_none_or(|new| new.is_dir() != old.is_dir()))
            {
                removals.push(CacheChange::Remove(path.clone()));
            }
            if let Some(new) = new {
                if new.is_file() {
                    total = total.checked_add(new.len()).ok_or(ToolError::Limit)?;
                }
                if !old.is_some_and(|old| same(&old, &new)) {
                    additions.push(if new.is_dir() {
                        CacheChange::Directory(path)
                    } else {
                        CacheChange::File(path)
                    });
                }
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

fn same(old: &Metadata, new: &Metadata) -> bool {
    old.is_dir() == new.is_dir()
        && (new.is_dir()
            || (old.len(), old.mtime(), old.mtime_nsec(), old.mode() & 0o111)
                == (new.len(), new.mtime(), new.mtime_nsec(), new.mode() & 0o111))
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

// Safe output outside the cache roots is validated and discarded; links, special and set-ID files refuse.
fn validate(path: &str, metadata: &Metadata) -> Result<(), ToolError> {
    if path.split('/').count() > COMMAND_SNAPSHOT_DEPTH {
        return Err(ToolError::Limit);
    }
    if if selected(path) {
        crate::store::validate_cache_path(path).is_err()
    } else {
        !valid_relative(path, false)
    } || metadata.mode() & 0o6000 != 0
        || !(metadata.is_dir() || metadata.is_file())
    {
        return Err(ToolError::Path);
    }
    Ok(())
}

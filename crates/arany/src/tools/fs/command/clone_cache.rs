use super::*;
use crate::store::{CACHE_ROOTS, CacheChange};
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
    let before = inventory(lower, exclusions, deadline)?;
    let after = inventory(view, exclusions, deadline)?;
    let mut changes = before
        .iter()
        .filter(|(path, old)| {
            after
                .get(*path)
                .is_none_or(|new| new.is_dir() != old.is_dir())
        })
        .map(|(path, _)| CacheChange::Remove(path.clone()))
        .rev()
        .collect::<Vec<_>>();
    let mut total = 0u64;
    for (path, entry) in &after {
        Selector::new(0, deadline).check_time()?;
        if entry.is_file() {
            total = total.checked_add(entry.len()).ok_or(ToolError::Limit)?;
        }
        if before.get(path).is_some_and(|old| same(old, entry)) {
            continue;
        }
        changes.push(if entry.is_dir() {
            CacheChange::Directory(path.clone())
        } else {
            CacheChange::File(path.clone())
        });
    }
    if total > limit {
        return Err(ToolError::Limit);
    }
    Ok(changes)
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
fn inventory(
    root: &Dir,
    exclusions: &[String],
    deadline: Instant,
) -> Result<BTreeMap<String, Metadata>, ToolError> {
    let mut budget = CommandScanBudget::default();
    let mut pending = vec![(
        String::new(),
        root.try_clone().map_err(|_| ToolError::Path)?,
    )];
    let mut entries = BTreeMap::new();
    while let Some((path, directory)) = pending.pop() {
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            Selector::new(0, deadline).check_time()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            budget.entry(&path)?;
            if excluded(&path, exclusions) {
                continue;
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
            let metadata = directory
                .symlink_metadata(&name)
                .map_err(|_| ToolError::Path)?;
            if metadata.mode() & 0o6000 != 0 || !(metadata.is_dir() || metadata.is_file()) {
                return Err(ToolError::Path);
            }
            if metadata.is_dir() {
                let child = directory
                    .open_dir_nofollow(&name)
                    .map_err(|_| ToolError::Path)?;
                pending.push((path.clone(), child));
            }
            if selected(&path) {
                entries.insert(path, metadata);
            }
        }
    }
    Ok(entries)
}

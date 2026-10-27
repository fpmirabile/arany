use super::*;
use crate::store::{CACHE_ROOTS, CacheChange, MAX_ARCHIVE_ENTRIES};
use std::os::unix::fs::FileTypeExt;

#[cfg(test)]
mod tests;

pub(in crate::tools) const CAPTURE_TIME: std::time::Duration = std::time::Duration::from_secs(5);

pub(in crate::tools) fn capture(
    lower: &Dir,
    upper: &Dir,
    exclusions: &[String],
    deadline: Instant,
    limit: u64,
) -> Result<Vec<CacheChange>, ToolError> {
    let mut budget = CommandScanBudget::default();
    let before = inventory(lower, Layer::Lower, exclusions, &mut budget, deadline)?;
    let after = inventory(upper, Layer::Upper, exclusions, &mut budget, deadline)?;
    let mut removals = BTreeSet::new();
    let mut additions = Vec::new();
    let mut sizes = before
        .iter()
        .filter(|(path, (entry, _))| {
            selected(path) && !excluded(path, exclusions) && entry.is_file()
        })
        .map(|(path, (entry, _))| (path.clone(), entry.len()))
        .collect::<BTreeMap<_, _>>();
    for (path, (entry, opaque)) in &after {
        Selector::new(0, deadline).check_time()?;
        if excluded(path, exclusions) {
            continue;
        }
        let whiteout = entry.file_type().is_char_device() && entry.rdev() == 0;
        if *opaque
            || whiteout
            || entry.is_file() && before.get(path).is_some_and(|(old, _)| old.is_dir())
            || entry.is_dir() && before.get(path).is_some_and(|(old, _)| old.is_file())
        {
            let prefix = format!("{path}/");
            let subtree = before.get_key_value(path).into_iter().chain(
                before
                    .range(prefix.clone()..)
                    .take_while(|(child, _)| child.starts_with(&prefix)),
            );
            for (child, _) in subtree {
                Selector::new(0, deadline).check_time()?;
                if selected(child)
                    && !excluded(child, exclusions)
                    && !exclusions
                        .iter()
                        .any(|root| Path::new(root).starts_with(child))
                {
                    removals.insert(child.clone());
                    sizes.remove(child);
                }
            }
        }
        if !selected(path) {
            continue;
        }
        if entry.is_file() {
            sizes.insert(path.clone(), entry.len());
            additions.push(CacheChange::File(path.clone()));
        } else if entry.is_dir() {
            additions.push(CacheChange::Directory(path.clone()));
        }
    }
    let total = sizes.values().try_fold(0u64, |sum, bytes| {
        sum.checked_add(*bytes).ok_or(ToolError::Limit)
    })?;
    if total > limit
        || sizes.len() > MAX_ARCHIVE_ENTRIES
        || removals.len() + additions.len() > MAX_ARCHIVE_ENTRIES * 2
    {
        return Err(ToolError::Limit);
    }
    let mut changes = removals
        .into_iter()
        .rev()
        .map(CacheChange::Remove)
        .collect::<Vec<_>>();
    changes.extend(additions);
    Selector::new(0, deadline).check_time()?;
    Ok(changes)
}

enum Layer {
    Lower,
    Upper,
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

fn inventory(
    root: &Dir,
    layer: Layer,
    exclusions: &[String],
    budget: &mut CommandScanBudget,
    deadline: Instant,
) -> Result<BTreeMap<String, (std::fs::Metadata, bool)>, ToolError> {
    let mut pending = vec![String::new()];
    let mut entries = BTreeMap::new();
    let mut cache_entries = 0usize;
    if matches!(layer, Layer::Upper) && metadata::validate(&pin(root, "")?, budget)? {
        return Err(ToolError::Path);
    }
    while let Some(path) = pending.pop() {
        Selector::new(0, deadline).check_time()?;
        let dir = Dir::from_std_file(readable(root, &path)?);
        for entry in dir.entries().map_err(|_| ToolError::Path)? {
            Selector::new(0, deadline).check_time()?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            let path = joined(&path, &name);
            budget.entry(&path)?;
            if selected(&path) && !excluded(&path, exclusions) {
                cache_entries += 1;
            }
            if cache_entries > MAX_ARCHIVE_ENTRIES
                || path.split('/').count() > COMMAND_SNAPSHOT_DEPTH
            {
                return Err(ToolError::Limit);
            }
            if if selected(&path) {
                crate::store::validate_cache_path(&path).is_err()
            } else {
                !valid_relative(&path, false)
            } {
                return Err(ToolError::Path);
            }
            let handle = pin(root, &path)?;
            let entry = handle.metadata().map_err(|_| ToolError::Path)?;
            if entry.mode() & 0o6000 != 0
                || !(entry.is_dir()
                    || entry.is_file()
                    || matches!(layer, Layer::Upper)
                        && entry.file_type().is_char_device()
                        && entry.rdev() == 0)
            {
                return Err(ToolError::Path);
            }
            let opaque = matches!(layer, Layer::Upper) && metadata::validate(&handle, budget)?;
            if entry.is_dir() {
                pending.push(path.clone());
            }
            if entry.is_dir() || entry.is_file() {
                drop(readable(root, &path)?);
            }
            entries.insert(path, (entry, opaque));
        }
    }
    Ok(entries)
}

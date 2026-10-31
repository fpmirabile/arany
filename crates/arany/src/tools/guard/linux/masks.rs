use crate::tools::ToolError;
use crate::tools::guard_settings::MAX_READ_FOLDER_ENTRIES;
use crate::tools::toolchains::{GrantMask, RuntimeGrant, RuntimeKind};
use crate::tools::types::{CommandResult, ToolCall, ToolObservation};
use std::path::{Path, PathBuf};

// Hides protected names at the first level of each approved read folder; a folder over the entry
// bound, or holding a protected-named link, is withheld from this command, never exposed unread.
pub(in crate::tools::guard) fn mask(grants: Vec<RuntimeGrant>) -> Vec<RuntimeGrant> {
    grants
        .into_iter()
        .filter_map(|grant| {
            if grant.kind != RuntimeKind::ExtraRead {
                return Some(grant);
            }
            Some(RuntimeGrant {
                masks: first_level(&grant.path)?,
                ..grant
            })
        })
        .collect()
}

fn first_level(folder: &Path) -> Option<Vec<GrantMask>> {
    #[cfg(any(test, feature = "test-support"))]
    if crate::fault::FaultPlan::mask_scan_refused(folder) {
        return None;
    }
    let mut masks = Vec::new();
    for (count, entry) in std::fs::read_dir(folder).ok()?.enumerate() {
        if count >= MAX_READ_FOLDER_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        let kind = entry.file_type().ok()?;
        if crate::validate::forbidden(&entry.file_name().to_string_lossy()) {
            if kind.is_symlink() {
                return None;
            }
            masks.push(GrantMask {
                path: entry.path(),
                directory: kind.is_dir(),
            });
        }
    }
    Some(masks)
}

pub(super) fn notice(
    mut observation: ToolObservation,
    hidden: &[PathBuf],
) -> Result<ToolObservation, ToolError> {
    if hidden.is_empty() || !matches!(observation.intent.call, ToolCall::Command { .. }) {
        return Ok(observation);
    }
    let Ok(mut result) = serde_json::from_str::<CommandResult>(&observation.output) else {
        return Ok(observation);
    };
    result.hidden_grants = hidden
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if !result.valid() {
        return Err(ToolError::Limit);
    }
    observation.output = serde_json::to_string(&result).map_err(|_| ToolError::Operation)?;
    Ok(observation)
}

use super::escape_terminal;
use arany::{StorageReport, StorageStatus, StorageUsage};
use std::fmt::Write;

pub const STORAGE_OUTPUT_LIMIT: usize = 64 * 1024;
const COVERAGE: &str = "Coverage: selected/default Session roots and fixed OS-user account root; identical roots counted once.\nPrivate records include secret-containing files only in aggregate; no names or contents.\nExcluded: main Workspace, unregistered StateRoots, developer checkouts, external worktrees, shared Cargo/npm caches, installed binaries and OS keyring.\nGuard backing storage/scratch: not yet included (storage slice S2).\nWorktrees: no owned-worktree registry in this version.\nPoint-in-time metadata; concurrent activity may change sizes. Allocated bytes are not guaranteed reclaimable (sparse files, compression, clones, snapshots). Reservations are not allocated capacity; only their record bytes count.\nCleanup: /cache clear covers this Workspace; /cache clear all covers all private caches in the selected StateRoot. Busy caches remain retained.\nGuard maintenance: docs/tools.md#historical-linux-seed-maintenance (independent ownership/inactivity evidence required).\nHistory, credentials and live/uncertain Guard storage have no automatic-removal hint.\n";

pub fn render_storage(report: &StorageReport) -> (String, bool) {
    let mut complete = report.complete() && report.categories.len() <= 128;
    let summaries = report
        .categories
        .iter()
        .take(128)
        .map(|row| {
            let status = match row.status {
                StorageStatus::Complete => "complete",
                StorageStatus::Partial => "partial",
                StorageStatus::Absent => "absent",
                StorageStatus::Busy => "busy",
                StorageStatus::Unavailable => "unavailable",
            };
            let mut summary = format!("{}: {status}\n  {}\n", row.name, usage(row.usage));
            if let Some(reason) = row.reason {
                let _ = writeln!(summary, "  {reason}");
            }
            summary
        })
        .collect::<Vec<_>>();
    let mut detail_room =
        (STORAGE_OUTPUT_LIMIT - 4096).saturating_sub(summaries.iter().map(String::len).sum());
    let mut body = String::new();
    let mut detail_overflow = false;
    for (row, summary) in report.categories.iter().zip(summaries) {
        body.push_str(&summary);
        if let Some(path) = &row.path {
            let detail = format!("  Location: {}\n", escape_terminal(&path.to_string_lossy()));
            if detail.len() <= detail_room {
                detail_room -= detail.len();
                body.push_str(&detail);
            } else {
                detail_overflow = true;
                complete = false;
            }
        }
    }
    if detail_overflow {
        body.push_str("Partial: output limit reached; location detail omitted; category summaries retained.\n");
    }
    if report.categories.len() > 128 {
        body.push_str("Partial: detail row limit reached; remaining detail omitted.\n");
    }
    let status = if complete {
        "complete for covered locations"
    } else {
        "partial; known subtotal only"
    };
    (
        format!(
            "Arany storage: {status}\nKnown owned subtotal: {}\n{body}{COVERAGE}",
            usage(report.subtotal)
        ),
        complete,
    )
}

fn usage(value: StorageUsage) -> String {
    format!(
        "allocated {} B; logical {} B; {} files; {} directories",
        value.allocated_bytes, value.logical_bytes, value.files, value.directories
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::StorageCategory;

    #[test]
    fn local_storage_output_escapes_owned_paths_and_reserves_coverage_when_full() {
        let row = || StorageCategory {
            name: "Private storage",
            path: Some(std::path::PathBuf::from(format!(
                "/private/{}",
                "\u{1b}".repeat(4096)
            ))),
            status: StorageStatus::Complete,
            usage: StorageUsage::default(),
            reason: None,
        };
        let report = StorageReport {
            categories: (0..6).map(|_| row()).collect(),
            subtotal: StorageUsage {
                logical_bytes: 42,
                ..StorageUsage::default()
            },
        };
        let (text, success) = render_storage(&report);
        assert!(!success);
        assert!(text.len() <= STORAGE_OUTPUT_LIMIT);
        assert!(text.contains("output limit reached"));
        assert_eq!(text.matches("Private storage: complete").count(), 6);
        assert!(text.contains("Known owned subtotal: allocated 0 B; logical 42 B"));
        assert!(text.ends_with(COVERAGE));
        assert!(!text.contains('\u{1b}'));
        let report = StorageReport {
            categories: (0..129)
                .map(|_| StorageCategory {
                    path: None,
                    ..row()
                })
                .collect(),
            subtotal: StorageUsage::default(),
        };
        let (text, success) = render_storage(&report);
        assert!(!success);
        assert!(text.contains("detail row limit reached"));
    }
}

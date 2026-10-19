use super::escape_terminal;
use arany::rewind::{CheckpointTarget, NonRewindable, RewindFinished, RewindOutcome};
use arany::{FileChange, FileVersion, SessionView};
use arany::{RewindIssue, RewindPreview};

pub const KEEP: &str = "Restore the others, keep these files";
pub const OVERWRITE: &str = "Overwrite changed files (their current content is saved first)";
pub const COMPARE_PAGE_BYTES: usize = 64 * 1024;

pub fn file_noun(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

pub fn checkpoint_label(view: &SessionView, target: &CheckpointTarget) -> String {
    match target {
        CheckpointTarget::Run { run_id, .. } => {
            let index = view
                .runs
                .iter()
                .position(|run| run.id == *run_id)
                .unwrap_or(0);
            format!("Run {}", index + 1)
        }
        CheckpointTarget::Rewind { .. } => "Before rewind".into(),
    }
}

pub fn issue_label(issue: RewindIssue) -> &'static str {
    match issue {
        RewindIssue::Drifted => "drifted",
        RewindIssue::HistoryDrift => "history drift",
        RewindIssue::Unavailable => "unavailable (pre-image pruned or not saved)",
        RewindIssue::Unsafe => "unsafe",
        RewindIssue::OutsideGrant => "outside grant",
    }
}

pub fn file_state(preview: &RewindPreview, index: usize) -> &'static str {
    let file = &preview.files()[index];
    if file.kept {
        "kept"
    } else if let Some(issue) = file.issues.first() {
        issue_label(*issue)
    } else if let Some(plan) = preview.plan().files.iter().find(|p| p.path == file.path) {
        if plan.restore_to.is_none() {
            "delete"
        } else if file.observed.is_none() {
            "recreate"
        } else {
            "restore"
        }
    } else {
        "kept"
    }
}

fn text(version: Option<&FileVersion>) -> Option<String> {
    match version {
        Some(version) => String::from_utf8(version.bytes_with_limit(1024 * 1024)?).ok(),
        None => Some(String::new()),
    }
}

fn changed_lines<'a>(before: &'a str, after: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (
        old[prefix..old.len() - suffix].to_vec(),
        new[prefix..new.len() - suffix].to_vec(),
    )
}

pub fn line_counts(change: &FileChange) -> Option<(u32, u32)> {
    let before = text(change.before.as_ref())?;
    let after = text(change.after.as_ref())?;
    let (old, new) = changed_lines(&before, &after);
    Some((
        u32::try_from(new.len()).ok()?,
        u32::try_from(old.len()).ok()?,
    ))
}

pub fn compare_pages(change: &FileChange) -> Vec<Vec<String>> {
    let (Some(before), Some(after)) = (text(change.before.as_ref()), text(change.after.as_ref()))
    else {
        return vec![vec![
            "Binary or oversized content; no text diff available.".into(),
        ]];
    };
    let (old, new) = changed_lines(&before, &after);
    let mut pages = vec![vec![
        format!("Compare: {}", escape_terminal(&change.path)),
        "- current Workspace; + checkpoint".into(),
    ]];
    let mut bytes = pages[0].iter().map(|line| line.len() + 1).sum::<usize>();
    for (prefix, lines) in [("-", old), ("+", new)] {
        for line in lines {
            let safe = escape_terminal(line.strip_suffix('\n').unwrap_or(line));
            let mut offset = 0;
            while offset < safe.len() {
                let mut end = (offset + COMPARE_PAGE_BYTES - 2).min(safe.len());
                while !safe.is_char_boundary(end) {
                    end -= 1;
                }
                let part = format!("{prefix}{}", &safe[offset..end]);
                if bytes + part.len() + 1 > COMPARE_PAGE_BYTES {
                    pages.push(Vec::new());
                    bytes = 0;
                }
                bytes += part.len() + 1;
                pages.last_mut().expect("comparison page").push(part);
                offset = end;
            }
            if safe.is_empty() {
                if bytes + 2 > COMPARE_PAGE_BYTES {
                    pages.push(Vec::new());
                    bytes = 0;
                }
                pages
                    .last_mut()
                    .expect("comparison page")
                    .push(prefix.into());
                bytes += 2;
            }
        }
    }
    if before == after {
        pages[0].push("Content unchanged (file mode may change).".into());
    }
    pages
}

pub fn preview_lines(preview: &RewindPreview) -> Vec<String> {
    let mut lines = vec![format!(
        "{} {} in the restore plan",
        preview.plan().files.len(),
        file_noun(preview.plan().files.len())
    )];
    for sequence in &preview.plan().blockers {
        lines.push(format!("Code restore blocked: publication at Event {sequence} has an unknown or open outcome; inspect files. Conversation restore remains available."));
    }
    for (index, file) in preview.files().iter().enumerate() {
        let counts = preview
            .comparisons()
            .iter()
            .find(|c| c.path == file.path)
            .and_then(line_counts)
            .map_or_else(|| "+? -?".into(), |(a, r)| format!("+{a} -{r}"));
        lines.push(format!(
            "{} · {} · {counts}",
            escape_terminal(&file.path),
            file_state(preview, index)
        ));
        for issue in file.issues.iter().skip(1) {
            lines.push(format!("  {}", issue_label(*issue)));
        }
    }
    lines.extend(non_rewindable_lines(preview.plan()));
    let conflicts = preview
        .files()
        .iter()
        .filter(|f| !f.kept && !f.issues.is_empty())
        .count();
    if conflicts != 0 {
        lines.push(format!("Rewind refused: {conflicts} {} changed outside Arany or cannot be restored safely. No file changed.", file_noun(conflicts)));
        lines.extend([KEEP.into(), OVERWRITE.into()]);
    }
    lines
}

pub fn non_rewindable_lines(plan: &arany::rewind::RewindPlan) -> Vec<String> {
    plan.non_rewindable
        .iter()
        .map(|(sequence, effect)| {
            let label = match effect {
                NonRewindable::UnguardedCommand => "unguarded command",
                NonRewindable::Mcp => "MCP external effect",
                NonRewindable::Cache => "build cache publication",
                NonRewindable::Directory => "empty directory",
            };
            format!("Not rewindable: {label} at Event {sequence}")
        })
        .collect()
}

pub fn receipt(finished: &RewindFinished, count: usize, label: &str) -> String {
    let mut result = match finished.outcome {
        RewindOutcome::Restored => {
            format!("Restored {count} {} to before {label}", file_noun(count))
        }
        RewindOutcome::Uncertain => {
            "Rewind outcome unknown: inspect files before continuing".into()
        }
        RewindOutcome::Conflict => format!(
            "Rewind refused: {} {} changed outside Arany",
            finished.conflict_count,
            file_noun(finished.conflict_count as usize)
        ),
        RewindOutcome::Denied => "Rewind denied; no files restored".into(),
        RewindOutcome::Failed => "Rewind failed; no files restored".into(),
        RewindOutcome::Cancelled => "Rewind cancelled; no files restored".into(),
    };
    for path in &finished.conflicts {
        result.push_str(&format!("\nConflict: {}", escape_terminal(path)));
    }
    if let Some(checkpoint) = &finished.checkpoint {
        if let Some(notice) = checkpoint.notice() {
            result.push_str(&format!("\n{notice}"));
        }
        if let Some(run) = checkpoint.pruned_before_run {
            result.push_str(&format!(
                "\nOlder checkpoints pruned; code restore unavailable before Run {run}"
            ));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    fn version(bytes: &[u8]) -> FileVersion {
        FileVersion {
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            content: base64::engine::general_purpose::STANDARD.encode(bytes),
            executable: false,
        }
    }

    #[test]
    fn rewind_comparison_and_receipts_escape_hostile_text_and_bound_every_page() {
        for (old, new, counts) in [
            ("same\nold\nend\n", "same\nnew\nend\n", (1, 1)),
            ("", "new\n", (1, 0)),
            ("old\n", "", (0, 1)),
            ("same", "same", (0, 0)),
        ] {
            let change = FileChange {
                path: "src/\u{1b}]52;clipboard\u{7}\r\u{202e}path".into(),
                before: Some(version(old.as_bytes())),
                after: Some(version(new.as_bytes())),
            };
            assert_eq!(line_counts(&change), Some(counts));
            for page in compare_pages(&change) {
                assert!(page.iter().map(|s| s.len() + 1).sum::<usize>() <= COMPARE_PAGE_BYTES);
                assert!(
                    page.iter()
                        .all(|line| !line.chars().any(|c| c.is_control() || c == '\u{202e}'))
                );
            }
        }
        let change = FileChange {
            path: "src/a".into(),
            before: Some(version(&vec![0x1b; 1024 * 1024])),
            after: None,
        };
        let pages = compare_pages(&change);
        assert!(pages.len() > 1);
        assert!(
            pages
                .iter()
                .all(|page| page.iter().map(|s| s.len() + 1).sum::<usize>() <= COMPARE_PAGE_BYTES)
        );
        for bytes in [vec![0xff], vec![b'a'; 1024 * 1024 + 1]] {
            let change = FileChange {
                path: "src/a".into(),
                before: Some(version(&bytes)),
                after: None,
            };
            assert_eq!(line_counts(&change), None);
            assert_eq!(
                compare_pages(&change),
                vec![vec!["Binary or oversized content; no text diff available."]]
            );
        }
        let mut finished = RewindFinished {
            rewind_id: uuid::Uuid::now_v7(),
            outcome: RewindOutcome::Uncertain,
            attestation: None,
            conflicts: vec!["src/\u{1b}[31m".into()],
            conflict_count: 1,
            checkpoint: None,
        };
        assert_eq!(
            receipt(&finished, 1, "Run 2"),
            "Rewind outcome unknown: inspect files before continuing\nConflict: src/\\u{001b}[31m"
        );
        for (count, noun) in [(0, "files"), (1, "file"), (2, "files")] {
            finished.conflict_count = count as u32;
            finished.outcome = RewindOutcome::Conflict;
            assert!(receipt(&finished, count, "Run 2").starts_with(&format!(
                "Rewind refused: {count} {noun} changed outside Arany"
            )));
            finished.outcome = RewindOutcome::Restored;
            assert!(
                receipt(&finished, count, "Run 2")
                    .starts_with(&format!("Restored {count} {noun} to before Run 2"))
            );
        }
    }
}

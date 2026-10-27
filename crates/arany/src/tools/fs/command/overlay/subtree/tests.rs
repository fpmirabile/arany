use super::super::tests::baseline;
use super::*;

fn opaque(path: &Path) {
    rustix::fs::setxattr(
        path,
        "user.overlay.opaque",
        b"y",
        rustix::fs::XattrFlags::CREATE,
    )
    .unwrap();
}

#[test]
fn subtree_capture_refuses_links_special_entries_and_file_directory_conversion() {
    for case in ["symlink", "ignored-symlink", "fifo", "type-change"] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        if case == "type-change" {
            std::fs::write(lower.path().join("tree"), b"before").unwrap();
        } else {
            std::fs::create_dir(lower.path().join("tree")).unwrap();
            let path = lower.path().join("tree/unsupported");
            if case == "fifo" {
                rustix::fs::mknodat(
                    rustix::fs::CWD,
                    path,
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::RUSR,
                    0,
                )
                .unwrap();
            } else {
                std::os::unix::fs::symlink("target", path).unwrap();
            }
            if case == "ignored-symlink" {
                std::fs::write(lower.path().join(".gitignore"), b"tree/unsupported\n").unwrap();
            }
        }
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        std::fs::create_dir(upper.path().join("tree")).unwrap();
        opaque(&upper.path().join("tree"));
        assert!(
            matches!(
                baseline.capture(
                    &source,
                    &open_directory(upper.path()).unwrap(),
                    Instant::now() + crate::tools::selection::TEST_RUNTIME,
                ),
                Err(ToolError::Path)
            ),
            "{case}"
        );
    }
}

#[test]
fn opaque_removal_and_recreation_use_frozen_baseline_file_preconditions() {
    for case in [
        "remove",
        "recreate",
        "ignored-recreation",
        "nested",
        "root",
        "new-directory",
        "publication",
    ] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lower.path().join("tree/nested")).unwrap();
        std::fs::write(lower.path().join("tree/nested/removed"), b"before").unwrap();
        std::fs::write(lower.path().join("tree/kept"), b"old").unwrap();
        std::fs::write(lower.path().join("tree/ignored"), b"private").unwrap();
        std::fs::write(lower.path().join("tree/ignored-removed"), b"private").unwrap();
        std::fs::write(lower.path().join(".gitignore"), b"tree/ignored*\n").unwrap();
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        let directory = if case == "new-directory" {
            "new"
        } else {
            "tree"
        };
        std::fs::create_dir(upper.path().join(directory)).unwrap();
        if case == "root" {
            opaque(upper.path());
            std::fs::copy(
                lower.path().join(".gitignore"),
                upper.path().join(".gitignore"),
            )
            .unwrap();
        } else {
            opaque(&upper.path().join(directory));
        }
        if case == "nested" {
            std::fs::create_dir(upper.path().join("tree/nested")).unwrap();
            opaque(&upper.path().join("tree/nested"));
        }
        if case == "ignored-recreation" {
            std::fs::write(upper.path().join("tree/ignored"), b"discarded replacement").unwrap();
        }
        if case != "remove" {
            std::fs::write(upper.path().join(format!("{directory}/kept")), [0, 255]).unwrap();
            std::fs::set_permissions(
                upper.path().join(format!("{directory}/kept")),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let (changes, summary) = baseline
            .capture(
                &source,
                &open_directory(upper.path()).unwrap(),
                Instant::now() + crate::tools::selection::TEST_RUNTIME,
            )
            .unwrap();
        check_recreation(lower.path(), &source, changes, summary, case);
    }
}

fn check_recreation(
    lower: &Path,
    source: &Dir,
    changes: CommandChangeset,
    summary: CommandSelectionSummary,
    case: &str,
) {
    assert_eq!(
        summary.discarded_outputs,
        2 * u64::from(case != "new-directory")
    );
    assert_eq!(
        changes.files.len(),
        if case == "new-directory" { 1 } else { 2 }
    );
    assert_eq!(
        changes.files.last().unwrap().path,
        if case == "new-directory" {
            "new/kept"
        } else {
            "tree/nested/removed"
        }
    );
    if case == "publication" {
        std::fs::write(lower.join("tree/nested/removed"), b"host").unwrap();
        assert!(matches!(
            apply_files(source, &changes.files),
            Err(ToolError::Conflict)
        ));
        assert_eq!(std::fs::read(lower.join("tree/kept")).unwrap(), b"old");
        return;
    }
    apply_files(source, &changes.files).unwrap();
    assert_eq!(
        std::fs::read(lower.join("tree/ignored")).unwrap(),
        b"private"
    );
    assert_eq!(
        std::fs::read(lower.join("tree/ignored-removed")).unwrap(),
        b"private"
    );
    if case == "remove" {
        assert!(!lower.join("tree/kept").exists());
    } else {
        let directory = if case == "new-directory" {
            "new"
        } else {
            "tree"
        };
        assert_eq!(
            std::fs::read(lower.join(format!("{directory}/kept"))).unwrap(),
            [0, 255]
        );
        assert_ne!(
            std::fs::metadata(lower.join(format!("{directory}/kept")))
                .unwrap()
                .mode()
                & 0o111,
            0
        );
    }
    assert_eq!(
        lower.join("tree/nested/removed").exists(),
        case == "new-directory"
    );
}

#[test]
fn subtree_capture_refuses_lower_drift_masks_incomplete_inventory_and_overflow() {
    for case in [
        "edit",
        "create",
        "root-create",
        "remove",
        "mask",
        "incomplete",
        "file-limit",
        "content-limit",
        "scan-limit",
    ] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lower.path().join("tree/nested")).unwrap();
        std::fs::write(lower.path().join("tree/nested/removed"), b"before").unwrap();
        if case == "file-limit" {
            for index in 0..MAX_CHANGESET_FILES {
                std::fs::write(lower.path().join(format!("tree/file-{index}")), b"before").unwrap();
            }
        }
        if case == "content-limit" {
            std::fs::write(
                lower.path().join("tree/nested/removed"),
                vec![0; MAX_CHANGESET_BYTES + 1],
            )
            .unwrap();
        }
        let source = open_directory(lower.path()).unwrap();
        let mut baseline = baseline(&source);
        match case {
            "edit" => std::fs::write(lower.path().join("tree/nested/removed"), b"host").unwrap(),
            "create" => std::fs::write(lower.path().join("tree/nested/host"), b"host").unwrap(),
            "root-create" => std::fs::write(lower.path().join("host"), b"host").unwrap(),
            "remove" => std::fs::remove_file(lower.path().join("tree/nested/removed")).unwrap(),
            "mask" => baseline.mask(Path::new("tree/nested")),
            "incomplete" => {
                baseline.complete.remove("tree/nested");
            }
            _ => {}
        }
        std::fs::create_dir(upper.path().join("tree")).unwrap();
        opaque(&if case == "root-create" {
            upper.path().to_path_buf()
        } else {
            upper.path().join("tree")
        });
        let target = open_directory(upper.path()).unwrap();
        let deadline = Instant::now() + crate::tools::selection::TEST_RUNTIME;
        if case == "scan-limit" {
            assert_scan_limit(&baseline, &source, &target, deadline);
        } else {
            let result = baseline.capture(&source, &target, deadline);
            if matches!(case, "file-limit" | "content-limit") {
                assert!(matches!(result, Err(ToolError::Limit)), "{case}");
            } else {
                assert!(matches!(result, Err(ToolError::Conflict)), "{case}");
            }
        }
    }
}

fn assert_scan_limit(baseline: &Baseline, source: &Dir, target: &Dir, deadline: Instant) {
    for remaining in [0, 1, 2] {
        let mut totals = Totals::default();
        totals.budget.entries = COMMAND_SCAN_ENTRIES - remaining;
        let mut capture = Capture {
            baseline,
            lower: source,
            upper: target,
            prefix: "",
            selector: &mut Selector::new(0, deadline),
            totals: &mut totals,
            files: BTreeMap::new(),
            discarded: BTreeSet::new(),
            subtrees: BTreeSet::from(["tree".into()]),
        };
        if remaining < 2 {
            assert!(matches!(capture.remove_subtrees(), Err(ToolError::Limit)));
        } else {
            capture.remove_subtrees().unwrap();
            capture.check_subtrees().unwrap();
            assert_eq!(capture.totals.budget.entries, COMMAND_SCAN_ENTRIES);
            assert_eq!(capture.files.len(), 1);
        }
    }
}

#[test]
fn subtree_directory_open_refuses_replacement_or_removal_after_path_check() {
    for case in ["replace", "remove"] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::create_dir(lower.path().join("tree")).unwrap();
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        baseline.check(&source, "tree").unwrap();
        std::fs::rename(lower.path().join("tree"), lower.path().join("old")).unwrap();
        if case == "replace" {
            std::fs::create_dir(lower.path().join("tree")).unwrap();
        }
        let mut capture = Capture {
            baseline: &baseline,
            lower: &source,
            upper: &open_directory(upper.path()).unwrap(),
            prefix: "",
            selector: &mut Selector::new(0, Instant::now() + crate::tools::selection::TEST_RUNTIME),
            totals: &mut Totals::default(),
            files: BTreeMap::new(),
            discarded: BTreeSet::new(),
            subtrees: BTreeSet::new(),
        };
        assert!(
            matches!(capture.check_directory("tree"), Err(ToolError::Conflict)),
            "{case}"
        );
    }
}

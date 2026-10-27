use super::*;

fn replace_leaf(directory: &Dir, leaf: &str, phase: Phase) {
    if matches!(phase, Phase::BeforeClaim) {
        directory.write("replacement", b"host replacement").unwrap();
        directory.rename("replacement", directory, leaf).unwrap();
    }
}

fn edit_leaf(directory: &Dir, leaf: &str, phase: Phase) {
    if matches!(phase, Phase::BeforeClaim) {
        directory.write(leaf, b"host edit").unwrap();
    }
}

fn occupy_restore(directory: &Dir, leaf: &str, phase: Phase) {
    match phase {
        Phase::BeforeClaim => replace_leaf(directory, leaf, phase),
        Phase::Claimed => directory.write(leaf, b"newest host version").unwrap(),
    }
}

#[test]
fn deletion_revalidates_the_claimed_inode_and_restores_drift_without_overwrite() {
    for case in ["replace", "edit", "restore-conflict", "partial"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source"), b"before").unwrap();
        let directory = open_directory(root.path()).unwrap();
        let mut changes = vec![FileChange {
            path: "source".into(),
            before: Some(FileVersion::from_bytes(b"before", false)),
            after: None,
        }];
        if case == "partial" {
            changes.insert(
                0,
                FileChange {
                    path: "created".into(),
                    before: None,
                    after: Some(FileVersion::from_bytes(b"published", false)),
                },
            );
        }
        let control = MutationControl {
            removal_hook: Some(match case {
                "edit" => edit_leaf,
                "restore-conflict" => occupy_restore,
                _ => replace_leaf,
            }),
            ..Default::default()
        };
        let result = apply_files_controlled(&directory, &changes, &control);
        if matches!(case, "restore-conflict" | "partial") {
            assert!(matches!(result, Err(ToolError::Uncertain)), "{case}");
        } else {
            assert!(matches!(result, Err(ToolError::Conflict)), "{case}");
        }
        assert_eq!(
            std::fs::read(root.path().join("source")).unwrap(),
            match case {
                "edit" => b"host edit".as_slice(),
                "restore-conflict" => b"newest host version".as_slice(),
                _ => b"host replacement".as_slice(),
            },
            "{case}"
        );
        let retained = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(TEMPORARY_PREFIX)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            retained.len(),
            usize::from(case == "restore-conflict"),
            "{case}"
        );
        if let Some(path) = retained.first() {
            assert_eq!(
                std::fs::read(path.join(CAPTURED_LEAF)).unwrap(),
                b"host replacement"
            );
        }
        if case == "partial" {
            assert_eq!(
                std::fs::read(root.path().join("created")).unwrap(),
                b"published"
            );
        }
    }
}

#[test]
fn deletion_sync_failure_is_uncertain_after_the_checked_inode_is_removed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source"), b"before").unwrap();
    let directory = open_directory(root.path()).unwrap();
    let intent = uuid::Uuid::now_v7();
    crate::FaultPlan::fail_file_parent_sync(intent);
    let control = MutationControl {
        intent: Some(intent),
        ..Default::default()
    };
    let changes = [FileChange {
        path: "source".into(),
        before: Some(FileVersion::from_bytes(b"before", false)),
        after: None,
    }];
    assert!(matches!(
        apply_files_controlled(&directory, &changes, &control),
        Err(ToolError::Uncertain)
    ));
    assert!(!root.path().join("source").exists());
}

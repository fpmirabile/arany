use super::*;

fn baseline(root: &Dir) -> Baseline {
    fn walk(root: &Dir, path: &str, baseline: &mut Baseline) {
        let directory = if path.is_empty() {
            root.try_clone().unwrap()
        } else {
            Dir::from_std_file(open_file(root, path).unwrap())
        };
        for entry in directory.entries().unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let child = if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            };
            let metadata = root.symlink_metadata(&child).unwrap();
            baseline.record(&child, metadata.clone());
            if metadata.is_dir() {
                walk(root, &child, baseline);
            }
        }
        baseline.complete(Path::new(path)).unwrap();
    }
    let mut baseline = Baseline::default();
    walk(root, "", &mut baseline);
    baseline
        .freeze(root, Instant::now() + crate::tools::selection::PHASE_TIME)
        .unwrap();
    baseline
}

#[test]
fn capture_freezes_selection_preserves_binary_exec_and_checks_host_drift() {
    for drift in ["none", "edit", "create", "parent", "publication"] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::write(lower.path().join("existing"), b"before").unwrap();
        std::fs::write(lower.path().join(".gitignore"), b"ignored/\n").unwrap();
        std::fs::create_dir(lower.path().join("ignored")).unwrap();
        std::fs::write(lower.path().join("ignored/existing"), b"dependency").unwrap();
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        std::fs::write(upper.path().join("existing"), b"after").unwrap();
        std::fs::write(upper.path().join("created"), [0, 255]).unwrap();
        std::fs::set_permissions(
            upper.path().join("created"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(upper.path().join(".gitignore"), b"existing\n").unwrap();
        std::fs::create_dir(upper.path().join("ignored")).unwrap();
        std::fs::write(upper.path().join("ignored/existing"), b"discarded").unwrap();
        std::fs::write(upper.path().join("ignored/generated"), b"discarded").unwrap();
        match drift {
            "edit" => std::fs::write(lower.path().join("existing"), b"host").unwrap(),
            "create" => std::fs::write(lower.path().join("created"), b"host").unwrap(),
            "parent" => {
                std::fs::remove_file(upper.path().join("created")).unwrap();
                std::fs::create_dir(upper.path().join("new")).unwrap();
                std::fs::write(upper.path().join("new/file"), b"candidate").unwrap();
                std::fs::create_dir(lower.path().join("new")).unwrap();
            }
            _ => {}
        }
        let result = baseline.capture(
            &source,
            &open_directory(upper.path()).unwrap(),
            Instant::now() + crate::tools::selection::PHASE_TIME,
        );
        if matches!(drift, "edit" | "create" | "parent") {
            assert!(matches!(result, Err(ToolError::Conflict)), "{drift}");
            continue;
        }
        let (changeset, summary) = result.unwrap();
        assert_eq!(summary.ignored_entries, 2);
        assert_eq!(summary.pruned_directories, 0);
        assert_eq!(summary.oversized_inputs, 0);
        assert_eq!(summary.discarded_outputs, 3);
        assert_eq!(changeset.files.len(), 3);
        assert!(changeset.files[1].after.as_ref().unwrap().executable);
        assert_eq!(
            changeset.files[1].after.as_ref().unwrap().bytes().unwrap(),
            [0, 255]
        );
        if drift == "publication" {
            std::fs::write(lower.path().join("existing"), b"host").unwrap();
            assert!(apply_files(&source, &changeset.files).is_err());
            assert!(!lower.path().join("created").exists());
        }
    }
}

#[test]
fn capture_validates_ignored_metadata_before_discard_and_rejects_publishable_opaque() {
    for (name, value, ignored, accepted) in [
        ("user.overlay.opaque", b"y".as_slice(), true, true),
        ("user.overlay.opaque", b"y", false, false),
        ("user.overlay.opaque", b"x", true, false),
        ("user.overlay.metacopy", b"y", true, false),
        ("user.overlay.whiteout", b"", true, false),
        ("user.overlay.redirect", b"outside", true, false),
        ("user.overlay.impure", b"y", false, true),
        ("user.note", b"ordinary", false, true),
    ] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        if ignored {
            std::fs::write(lower.path().join(".gitignore"), b"output/\n").unwrap();
        }
        std::fs::create_dir(lower.path().join("output")).unwrap();
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        std::fs::create_dir(upper.path().join("output")).unwrap();
        rustix::fs::setxattr(
            upper.path().join("output"),
            name,
            value,
            rustix::fs::XattrFlags::CREATE,
        )
        .unwrap();
        let result = baseline.capture(
            &source,
            &open_directory(upper.path()).unwrap(),
            Instant::now() + crate::tools::selection::PHASE_TIME,
        );
        assert_eq!(result.is_ok(), accepted, "{name}, ignored={ignored}");
    }
    for kind in ["symlink", "hardlink", "setid", "fifo"] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::write(lower.path().join(".gitignore"), b"output/\n").unwrap();
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        std::fs::create_dir(upper.path().join("output")).unwrap();
        let path = upper.path().join("output/unsafe");
        match kind {
            "symlink" => std::os::unix::fs::symlink("outside", path).unwrap(),
            "hardlink" => {
                std::fs::write(upper.path().join("output/other"), b"data").unwrap();
                std::fs::hard_link(upper.path().join("output/other"), path).unwrap();
            }
            "setid" => {
                std::fs::write(&path, b"data").unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o4600)).unwrap();
            }
            "fifo" => rustix::fs::mknodat(
                rustix::fs::CWD,
                path,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::RUSR,
                0,
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(
            baseline
                .capture(
                    &source,
                    &open_directory(upper.path()).unwrap(),
                    Instant::now() + crate::tools::selection::PHASE_TIME
                )
                .is_err(),
            "{kind}"
        );
    }
}

#[test]
fn capture_restores_private_readability_and_refuses_incomplete_absence_or_caps() {
    let lower = tempfile::tempdir().unwrap();
    let upper = tempfile::tempdir().unwrap();
    let source = open_directory(lower.path()).unwrap();
    let mut baseline = baseline(&source);
    std::fs::write(upper.path().join("created"), b"readable").unwrap();
    std::fs::set_permissions(
        upper.path().join("created"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let target = open_directory(upper.path()).unwrap();
    let deadline = Instant::now() + crate::tools::selection::PHASE_TIME;
    assert_eq!(
        baseline
            .capture(&source, &target, deadline)
            .unwrap()
            .0
            .files[0]
            .after
            .as_ref()
            .unwrap()
            .bytes()
            .unwrap(),
        b"readable"
    );
    assert_eq!(
        std::fs::metadata(upper.path().join("created"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    let deep = vec!["nested"; COMMAND_SNAPSHOT_DEPTH - 1].join("/");
    std::fs::create_dir_all(upper.path().join(&deep)).unwrap();
    std::fs::write(upper.path().join(deep).join("leaf"), b"deep content").unwrap();
    assert_eq!(
        baseline
            .capture(&source, &target, deadline)
            .unwrap()
            .0
            .files
            .len(),
        2
    );
    baseline.mask(Path::new(""));
    assert!(matches!(
        baseline.capture(&source, &target, deadline),
        Err(ToolError::Conflict)
    ));
    let baseline = self::baseline(&source);
    std::fs::write(
        upper.path().join("oversized"),
        vec![0; MAX_CHANGESET_BYTES + 1],
    )
    .unwrap();
    assert!(matches!(
        baseline.capture(&source, &target, deadline),
        Err(ToolError::Limit)
    ));
}

#[test]
fn identical_copyups_still_consume_candidate_caps() {
    for files in [1, MAX_CHANGESET_FILES + 1] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        let content = vec![
            b'a';
            if files == 1 {
                MAX_CHANGESET_BYTES / 2 + 1
            } else {
                1
            }
        ];
        for index in 0..files {
            for root in [lower.path(), upper.path()] {
                std::fs::write(root.join(format!("file-{index}")), &content).unwrap();
            }
        }
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        assert!(matches!(
            baseline.capture(
                &source,
                &open_directory(upper.path()).unwrap(),
                Instant::now() + crate::tools::selection::PHASE_TIME
            ),
            Err(ToolError::Limit)
        ));
    }
}

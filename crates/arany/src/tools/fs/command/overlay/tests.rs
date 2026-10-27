use super::*;

pub(super) fn baseline(root: &Dir) -> Baseline {
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
fn capture_validates_ignored_and_publishable_metadata_before_selection() {
    for (name, value, ignored, accepted) in [
        ("user.overlay.opaque", b"y".as_slice(), true, true),
        ("user.overlay.opaque", b"y", false, true),
        ("user.overlay.opaque", b"x", true, false),
        ("user.overlay.opaque", b"x", false, false),
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
        match rustix::fs::setxattr(
            upper.path().join("output"),
            name,
            value,
            rustix::fs::XattrFlags::CREATE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::OPNOTSUPP) => {
                eprintln!("overlay xattr cases skipped: user xattrs are unsupported");
                break;
            }
            Err(error) => panic!("xattr fixture failed: {error}"),
        }
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
fn identical_copyups_do_not_consume_candidate_caps() {
    for files in [1, MAX_CHANGESET_FILES + 1] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        let bytes = if files == 1 {
            COMMAND_SNAPSHOT_BYTES + 1
        } else {
            1
        };
        for index in 0..files {
            for root in [lower.path(), upper.path()] {
                std::fs::File::create(root.join(format!("file-{index}")))
                    .unwrap()
                    .set_len(bytes as u64)
                    .unwrap();
            }
        }
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        let (changes, summary) = baseline
            .capture(
                &source,
                &open_directory(upper.path()).unwrap(),
                Instant::now() + crate::tools::selection::PHASE_TIME,
            )
            .unwrap();
        assert!(changes.files.is_empty());
        assert_eq!(summary.discarded_outputs, 0);
    }
}

#[test]
fn comparison_preserves_content_exec_drift_and_metadata_checks() {
    for case in [
        "equal",
        "content",
        "exec",
        "exec-removed",
        "drift",
        "metadata",
    ] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        std::fs::write(lower.path().join("existing"), [0, 255]).unwrap();
        let source = open_directory(lower.path()).unwrap();
        let path = upper.path().join("existing");
        std::fs::write(
            &path,
            if case == "content" {
                [0, 254]
            } else {
                [0, 255]
            },
        )
        .unwrap();
        for root in [lower.path(), upper.path()] {
            std::fs::set_permissions(
                root.join("existing"),
                std::fs::Permissions::from_mode(
                    if case == "exec-removed" && root == lower.path() {
                        0o700
                    } else {
                        0o600
                    },
                ),
            )
            .unwrap();
        }
        let baseline = baseline(&source);
        match case {
            "exec" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap()
            }
            "drift" => std::fs::write(lower.path().join("existing"), b"host").unwrap(),
            "metadata" => match rustix::fs::setxattr(
                &path,
                "user.overlay.whiteout",
                b"",
                rustix::fs::XattrFlags::CREATE,
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::OPNOTSUPP) => {
                    eprintln!("overlay metadata case skipped: user xattrs are unsupported");
                    continue;
                }
                Err(error) => panic!("xattr fixture failed: {error}"),
            },
            _ => {}
        }
        let result = baseline.capture(
            &source,
            &open_directory(upper.path()).unwrap(),
            Instant::now() + crate::tools::selection::PHASE_TIME,
        );
        match case {
            "drift" => assert!(matches!(result, Err(ToolError::Conflict))),
            "metadata" => assert!(matches!(result, Err(ToolError::Path))),
            _ => {
                let (changes, summary) = result.unwrap();
                assert_eq!(summary.discarded_outputs, 0);
                assert_eq!(changes.files.len(), usize::from(case != "equal"));
                if let Some(change) = changes.files.first() {
                    assert_eq!(change.after.as_ref().unwrap().executable, case == "exec");
                    assert_eq!(
                        change.before.as_ref().unwrap().executable,
                        case == "exec-removed"
                    );
                    apply_files(&source, &changes.files).unwrap();
                    assert_eq!(
                        std::fs::read(lower.path().join("existing")).unwrap(),
                        change.after.as_ref().unwrap().bytes().unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn plain_whiteouts_use_baseline_files_and_classify_host_creation_as_conflict() {
    for case in [
        "file",
        "directory",
        "symlink",
        "absent",
        "host-create",
        "content-limit",
        "file-limit",
    ] {
        let lower = tempfile::tempdir().unwrap();
        let upper = tempfile::tempdir().unwrap();
        let path = lower.path().join("removed");
        match case {
            "directory" => {
                std::fs::create_dir(&path).unwrap();
                std::fs::write(path.join("nested"), b"before").unwrap();
            }
            "symlink" => std::os::unix::fs::symlink("target", &path).unwrap(),
            "absent" | "host-create" => {}
            "content-limit" | "file-limit" => {
                let bytes = if case == "content-limit" {
                    MAX_CHANGESET_BYTES
                } else {
                    MAX_FILE_BYTES
                };
                std::fs::File::create(&path)
                    .unwrap()
                    .set_len(bytes as u64 + 1)
                    .unwrap();
            }
            _ => std::fs::write(&path, b"before").unwrap(),
        }
        let source = open_directory(lower.path()).unwrap();
        let baseline = baseline(&source);
        if case == "host-create" {
            std::fs::write(&path, b"host").unwrap();
        }
        match rustix::fs::mknodat(
            rustix::fs::CWD,
            upper.path().join("removed"),
            rustix::fs::FileType::CharacterDevice,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::PERM) => {
                eprintln!(
                    "whiteout capture cases skipped: mknod of character device 0/0 is not permitted"
                );
                return;
            }
            Err(error) => panic!("whiteout fixture failed: {error}"),
        }
        let result = baseline.capture(
            &source,
            &open_directory(upper.path()).unwrap(),
            Instant::now() + crate::tools::selection::PHASE_TIME,
        );
        match case {
            "file" | "directory" => {
                let (changes, _) = result.unwrap();
                assert_eq!(changes.files.len(), 1);
                let change = &changes.files[0];
                assert_eq!(
                    change.path,
                    if case == "directory" {
                        "removed/nested"
                    } else {
                        "removed"
                    }
                );
                assert_eq!(
                    change.before.as_ref().unwrap().sha256,
                    hex_digest(b"before")
                );
                assert_eq!(change.before.as_ref().unwrap().bytes().unwrap(), b"before");
                assert!(change.after.is_none());
                apply_files(&source, &changes.files).unwrap();
                assert!(!lower.path().join(&change.path).exists());
            }
            "host-create" => {
                assert!(matches!(result, Err(ToolError::Conflict)));
                assert_eq!(std::fs::read(path).unwrap(), b"host");
            }
            "content-limit" | "file-limit" => {
                assert!(matches!(result, Err(ToolError::Limit)), "{case}")
            }
            _ => assert!(matches!(result, Err(ToolError::Path)), "{case}"),
        }
    }
}

#[test]
fn multiple_roots_keep_relative_selection_and_share_candidate_limits() {
    for scenario in [
        "selection",
        "content-limit",
        "file-limit",
        "compare-limit",
        "compare-bound",
        "conflict",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut sources = Vec::new();
        let mut baselines = Vec::new();
        let deadline = Instant::now() + crate::tools::selection::PHASE_TIME;
        let mut selector = Selector::new(0, deadline);
        for (index, prefix) in ["src", "crates/core"].into_iter().enumerate() {
            let lower = directory.path().join(format!("lower-{index}"));
            let upper = directory.path().join(format!("upper-{index}"));
            std::fs::create_dir(&lower).unwrap();
            std::fs::create_dir(&upper).unwrap();
            std::fs::write(lower.join("existing"), b"before").unwrap();
            std::fs::write(lower.join(".gitignore"), b"/ignored\n").unwrap();
            let source = open_directory(&lower).unwrap();
            let mut baseline = baseline(&source);
            baseline.selection = CommandSelection::default();
            baseline.freeze_at(&source, prefix, &mut selector).unwrap();
            match scenario {
                "compare-limit" | "compare-bound" => {
                    std::fs::write(upper.join("existing"), b"before").unwrap()
                }
                "content-limit" => std::fs::write(
                    upper.join("created"),
                    vec![b'a'; MAX_CHANGESET_BYTES / 2 + 1],
                )
                .unwrap(),
                "file-limit" => {
                    for number in 0..=MAX_CHANGESET_FILES / 2 {
                        std::fs::write(upper.join(format!("file-{number}")), b"after").unwrap();
                    }
                }
                _ => {
                    std::fs::write(upper.join("existing"), b"after").unwrap();
                    std::fs::write(upper.join("ignored"), b"discarded").unwrap();
                }
            }
            if scenario == "conflict" && index == 1 {
                std::fs::write(lower.join("existing"), b"host").unwrap();
            }
            baselines.push(baseline);
            sources.push((source, open_directory(&upper).unwrap(), prefix));
        }
        let roots = sources
            .into_iter()
            .zip(&baselines)
            .map(|((lower, upper, prefix), baseline)| (baseline, lower, upper, prefix))
            .collect::<Vec<_>>();
        let upper_bytes = match scenario {
            "compare-limit" => (roots.len() * b"before".len() - 1) as u64,
            "compare-bound" => (roots.len() * b"before".len()) as u64,
            _ => DEFAULT_COMMAND_PROJECT_BYTES,
        };
        let captured = capture_roots(&roots, deadline, upper_bytes);
        match scenario {
            "content-limit" | "file-limit" | "compare-limit" => {
                assert!(matches!(captured, Err(ToolError::Limit)), "{scenario}")
            }
            "conflict" => assert!(matches!(captured, Err(ToolError::Conflict))),
            "compare-bound" => {
                let (changes, summary) = captured.unwrap();
                assert!(changes.files.is_empty());
                assert_eq!(summary.discarded_outputs, 0);
            }
            _ => {
                let (changes, summary) = captured.unwrap();
                assert_eq!(
                    changes
                        .files
                        .iter()
                        .map(|file| file.path.as_str())
                        .collect::<Vec<_>>(),
                    ["crates/core/existing", "src/existing"]
                );
                assert_eq!(summary.discarded_outputs, 2);
                assert!(
                    changes
                        .files
                        .iter()
                        .all(|file| file.before.as_ref().unwrap().bytes().unwrap() == b"before")
                );
            }
        }
    }
}

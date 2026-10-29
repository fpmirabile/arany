use crate::tools::config::Config;
use crate::tools::tests::config_value;
use crate::tools::types::*;
use serde_json::json;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn command_snapshots_exceed_previous_capacity_and_reject_new_limits() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for index in 0..=MAX_SNAPSHOT_FILES {
        std::fs::write(
            workspace.join(format!("source-{index:04}")),
            vec![b'x'; 16 * 1024],
        )
        .unwrap();
    }
    assert!(super::fs::command_snapshot(&workspace).is_ok());

    let many = temp.path().join("many");
    std::fs::create_dir(&many).unwrap();
    for index in 0..COMMAND_SNAPSHOT_FILES {
        std::fs::write(many.join(format!("entry-{index:05}")), b"").unwrap();
    }
    assert!(super::fs::command_snapshot(&many).is_ok());
    std::fs::write(many.join("overflow"), b"").unwrap();
    assert!(matches!(
        super::fs::command_snapshot(&many),
        Err(super::ToolError::Limit)
    ));

    let bytes = temp.path().join("bytes");
    std::fs::create_dir(&bytes).unwrap();
    for index in 0..COMMAND_SNAPSHOT_BYTES / MAX_FILE_BYTES {
        std::fs::write(
            bytes.join(format!("content-{index:03}")),
            vec![b'x'; MAX_FILE_BYTES],
        )
        .unwrap();
    }
    assert!(super::fs::command_snapshot(&bytes).is_ok());
    std::fs::write(bytes.join("overflow"), b"x").unwrap();
    assert!(matches!(
        super::fs::command_snapshot(&bytes),
        Err(super::ToolError::Limit)
    ));
}

#[test]
fn command_selection_freezes_rules_and_validates_ignored_output() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for case in [
        "selected",
        "collision",
        "ignored-link",
        "ignored-hardlink",
        "ignored-setid",
        "output-overflow",
        "baseline-overflow",
        "ignored-fifo",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let resources = temp.path().join("resources");
        for path in [&source, &target, &resources] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::create_dir(source.join("nested")).unwrap();
        std::fs::create_dir(source.join("pruned")).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("hidden"), b"excluded content").unwrap();
        symlink(&outside, source.join("target")).unwrap();
        symlink(outside.join("hidden"), source.join("result")).unwrap();
        std::fs::hard_link(outside.join("hidden"), source.join("ignored-hardlink.bin")).unwrap();
        nix::unistd::mkfifo(
            &source.join("ignored-fifo.bin"),
            nix::sys::stat::Mode::S_IRUSR,
        )
        .unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(source.join("ignored-socket.bin")).unwrap();
        std::fs::write(
            source.join(".gitignore"),
            b"*.bin\nresult\npruned/\n!pruned/visible\n.gitignore\n",
        )
        .unwrap();
        std::fs::write(source.join("nested/.gitignore"), b"!keep.bin\n").unwrap();
        std::fs::write(source.join("nested/keep.bin"), b"kept").unwrap();
        std::fs::write(source.join("nested/drop.bin"), b"omitted").unwrap();
        std::fs::write(source.join("pruned/visible"), b"not reopened").unwrap();
        std::fs::write(source.join("source.txt"), b"before").unwrap();
        std::fs::write(source.join("exact"), vec![b'x'; MAX_FILE_BYTES]).unwrap();
        std::fs::write(source.join("oversized"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        let mut config = config_value();
        config["workspace_paths"] = json!(["."]);
        let config: Config = serde_json::from_value(config).unwrap();
        let selection = super::fs::prepare_command(
            &super::fs::open_directory(&source).unwrap(),
            &super::fs::open_directory(&target).unwrap(),
            &super::fs::open_directory(&resources).unwrap(),
            &config,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        )
        .unwrap();
        assert!(selection.summary.elapsed_ms.is_some(), "{case}");
        assert!(selection.summary.valid(), "{case}");
        assert_eq!(selection.summary.oversized_inputs, 1, "{case}");
        assert_eq!(selection.summary.pruned_directories, 1, "{case}");
        for absent in [
            ".gitignore",
            "pruned",
            "nested/drop.bin",
            "oversized",
            "target",
            "result",
            "ignored-hardlink.bin",
            "ignored-fifo.bin",
            "ignored-socket.bin",
        ] {
            assert!(!target.join(absent).exists(), "{case}: {absent}");
        }
        assert_eq!(
            std::fs::metadata(target.join("exact")).unwrap().len(),
            MAX_FILE_BYTES as u64
        );
        assert_eq!(
            std::fs::read(target.join("nested/keep.bin")).unwrap(),
            b"kept"
        );
        let before = super::fs::command_snapshot(&target).unwrap();
        std::fs::write(target.join("source.txt"), b"after").unwrap();
        std::fs::write(target.join(".gitignore"), b"*\n").unwrap();
        std::fs::create_dir(target.join("pruned")).unwrap();
        std::fs::write(
            target.join("pruned/generated"),
            vec![b'g'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        match case {
            "collision" => std::fs::write(target.join("oversized"), b"replace").unwrap(),
            "ignored-link" => symlink("../source.txt", target.join("pruned/link")).unwrap(),
            "ignored-hardlink" => {
                std::fs::hard_link(target.join("source.txt"), target.join("pruned/link")).unwrap()
            }
            "ignored-setid" => std::fs::set_permissions(
                target.join("pruned/generated"),
                std::fs::Permissions::from_mode(0o4755),
            )
            .unwrap(),
            "output-overflow" => {
                std::fs::write(target.join("new"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap()
            }
            "baseline-overflow" => {
                std::fs::write(target.join("exact"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap()
            }
            "ignored-fifo" => {
                nix::unistd::mkfifo(&target.join("pruned/fifo"), nix::sys::stat::Mode::S_IRUSR)
                    .unwrap()
            }
            "selected" => {}
            _ => unreachable!(),
        }
        let captured = super::fs::selected_command_changeset(
            &target,
            &before,
            &selection,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        );
        if case == "selected" {
            let (changes, summary) = captured.unwrap();
            assert_eq!(summary.discarded_outputs, 2);
            assert_eq!(changes.files.len(), 1);
            assert_eq!(changes.files[0].path, "source.txt");
        } else {
            assert!(
                match case {
                    "collision" => matches!(captured, Err(super::ToolError::Conflict)),
                    "ignored-link" | "ignored-hardlink" | "ignored-fifo" =>
                        matches!(captured, Err(super::ToolError::Path)),
                    "ignored-setid" => matches!(captured, Err(super::ToolError::FilesystemDenied)),
                    "output-overflow" | "baseline-overflow" =>
                        matches!(captured, Err(super::ToolError::Limit)),
                    _ => unreachable!(),
                },
                "{case}"
            );
        }
        assert_eq!(std::fs::read(source.join("source.txt")).unwrap(), b"before");
    }
}

#[test]
fn command_selection_rejects_unexcluded_unsafe_inputs() {
    for kind in ["symlink", "hardlink", "fifo", "socket"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let resources = temp.path().join("resources");
        for path in [&source, &target, &resources] {
            std::fs::create_dir(path).unwrap();
        }
        let input = source.join("unsafe-input");
        let _socket = match kind {
            "symlink" => {
                std::os::unix::fs::symlink("missing", &input).unwrap();
                None
            }
            "hardlink" => {
                let outside = temp.path().join("outside");
                std::fs::write(&outside, b"content").unwrap();
                std::fs::hard_link(&outside, &input).unwrap();
                None
            }
            "fifo" => {
                nix::unistd::mkfifo(&input, nix::sys::stat::Mode::S_IRUSR).unwrap();
                None
            }
            "socket" => Some(std::os::unix::net::UnixListener::bind(&input).unwrap()),
            _ => unreachable!(),
        };
        let mut config = config_value();
        config["workspace_paths"] = json!(["."]);
        let config: Config = serde_json::from_value(config).unwrap();
        assert!(
            matches!(
                super::fs::prepare_command(
                    &super::fs::open_directory(&source).unwrap(),
                    &super::fs::open_directory(&target).unwrap(),
                    &super::fs::open_directory(&resources).unwrap(),
                    &config,
                    std::time::Instant::now() + super::selection::TEST_RUNTIME,
                ),
                Err(super::ToolError::Path)
            ),
            "{kind}"
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}

#[test]
fn command_selection_rejects_unsafe_or_unbounded_controls_before_copy() {
    use std::os::unix::fs::symlink;
    for case in [
        "link",
        "hardlink",
        "directory",
        "encoding",
        "file-bytes",
        "line",
        "stars",
        "recursive",
        "braces",
        "expansion",
        "aggregate-expansion",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let resources = temp.path().join("resources");
        for path in [&source, &target, &resources] {
            std::fs::create_dir(path).unwrap();
        }
        let control = source.join(".gitignore");
        match case {
            "link" => symlink("outside", &control).unwrap(),
            "hardlink" => {
                std::fs::write(source.join("other"), b"*\n").unwrap();
                std::fs::hard_link(source.join("other"), &control).unwrap();
            }
            "directory" => std::fs::create_dir(&control).unwrap(),
            "encoding" => std::fs::write(&control, [0xff]).unwrap(),
            "file-bytes" => std::fs::write(&control, vec![b'#'; 32 * 1024 + 1]).unwrap(),
            "line" => std::fs::write(&control, format!("#{}", "x".repeat(4096))).unwrap(),
            "stars" => std::fs::write(&control, "*x".repeat(17)).unwrap(),
            "recursive" => std::fs::write(&control, "**/".repeat(5)).unwrap(),
            "braces" => std::fs::write(&control, "{a,b}".repeat(5)).unwrap(),
            "expansion" => std::fs::write(&control, "x".repeat(1985)).unwrap(),
            "aggregate-expansion" => std::fs::write(&control, "x\n".repeat(505)).unwrap(),
            _ => unreachable!(),
        }
        std::fs::write(source.join("visible"), b"source").unwrap();
        let mut config = config_value();
        config["workspace_paths"] = json!(["."]);
        let config: Config = serde_json::from_value(config).unwrap();
        let result = super::fs::prepare_command(
            &super::fs::open_directory(&source).unwrap(),
            &super::fs::open_directory(&target).unwrap(),
            &super::fs::open_directory(&resources).unwrap(),
            &config,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        );
        assert!(
            match case {
                "link" | "hardlink" | "directory" | "encoding" =>
                    matches!(result, Err(super::ToolError::Path)),
                "file-bytes"
                | "line"
                | "stars"
                | "recursive"
                | "braces"
                | "expansion"
                | "aggregate-expansion" => matches!(result, Err(super::ToolError::Limit)),
                _ => unreachable!(),
            },
            "{case}"
        );
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            0,
            "{case}: no partial copy"
        );
    }
}

#[test]
fn command_controls_cannot_exceed_the_grant_or_path_depth() {
    for case in ["grant", "depth-limit", "depth-overflow"] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let resources = temp.path().join("resources");
        for path in [&source, &target, &resources] {
            std::fs::create_dir(path).unwrap();
        }
        let mut config = config_value();
        config["workspace_paths"] = json!(["."]);
        if case == "grant" {
            std::fs::write(source.join(".gitignore"), b"ignored").unwrap();
            std::fs::write(source.join("visible"), b"x").unwrap();
            config["workspace_paths"] = json!(["visible"]);
        } else {
            let depth = COMMAND_SNAPSHOT_DEPTH - usize::from(case == "depth-limit");
            let deep = source.join(vec!["nested"; depth].join("/"));
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(deep.join(".gitignore"), b"ignored").unwrap();
        }
        let config: Config = serde_json::from_value(config).unwrap();
        let result = super::fs::prepare_command(
            &super::fs::open_directory(&source).unwrap(),
            &super::fs::open_directory(&target).unwrap(),
            &super::fs::open_directory(&resources).unwrap(),
            &config,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        );
        if case == "grant" {
            assert!(matches!(result, Err(super::ToolError::FilesystemDenied)));
        } else if case == "depth-limit" {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(super::ToolError::Limit)));
        }
    }
}

#[test]
fn command_selection_rejects_an_overlong_relative_path() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    let resources = temp.path().join("resources");
    for path in [&source, &target, &resources] {
        std::fs::create_dir(path).unwrap();
    }
    let deep = source.join(format!("{}/{}", "a".repeat(200), "b".repeat(200)));
    std::fs::create_dir_all(deep.join("c".repeat(120))).unwrap();
    std::fs::write(deep.join("c".repeat(120)).join("file"), b"x").unwrap();
    let mut config = config_value();
    config["workspace_paths"] = json!(["."]);
    let config: Config = serde_json::from_value(config).unwrap();
    assert!(matches!(
        super::fs::prepare_command(
            &super::fs::open_directory(&source).unwrap(),
            &super::fs::open_directory(&target).unwrap(),
            &super::fs::open_directory(&resources).unwrap(),
            &config,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        ),
        Err(super::ToolError::Limit)
    ));
}

#[test]
fn command_selection_negations_cannot_disclose_protected_names() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    let resources = temp.path().join("resources");
    for path in [&source, &target, &resources] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(
        source.join(".gitignore"),
        b"!.env\n!.env.local\n!.git/\n!.ssh\n!**/.aws\n",
    )
    .unwrap();
    for file in [".env", ".env.local", "visible"] {
        std::fs::write(source.join(file), b"synthetic canary").unwrap();
    }
    for directory in [".git", ".ssh", "nested/.aws"] {
        std::fs::create_dir_all(source.join(directory)).unwrap();
        std::fs::write(source.join(directory).join("canary"), b"synthetic canary").unwrap();
    }
    let mut config = config_value();
    config["workspace_paths"] = json!(["."]);
    let result = super::fs::prepare_command(
        &super::fs::open_directory(&source).unwrap(),
        &super::fs::open_directory(&target).unwrap(),
        &super::fs::open_directory(&resources).unwrap(),
        &serde_json::from_value(config).unwrap(),
        std::time::Instant::now() + super::selection::TEST_RUNTIME,
    )
    .unwrap();
    for protected in [".env", ".env.local", ".git", ".ssh", "nested/.aws"] {
        assert!(
            !target.join(protected).exists(),
            "protected name was copied"
        );
    }
    assert!(target.join("visible").is_file());
    assert_eq!(result.summary.ignored_entries, 0);
}

#[test]
fn an_expired_selection_names_the_selection_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    let resources = temp.path().join("resources");
    for path in [&source, &target, &resources] {
        std::fs::create_dir(path).unwrap();
    }
    let mut config = config_value();
    config["workspace_paths"] = json!(["."]);
    let config: Config = serde_json::from_value(config).unwrap();
    let result = super::fs::prepare_command(
        &super::fs::open_directory(&source).unwrap(),
        &super::fs::open_directory(&target).unwrap(),
        &super::fs::open_directory(&resources).unwrap(),
        &config,
        std::time::Instant::now(),
    );
    assert!(matches!(
        result,
        Err(super::ToolError::ProjectSelection(
            super::SelectionCause::TimedOut
        ))
    ));
}

#[test]
fn command_selection_prunes_nested_repositories_and_discards_their_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    for (path, content) in [
        ("visible", "kept"),
        ("worktree/.git", "gitdir: elsewhere"),
        ("worktree/inner", "nested worktree"),
        ("repo/.git/HEAD", "ref: refs/heads/main"),
        ("repo/file", "nested repository"),
    ] {
        std::fs::create_dir_all(source.join(path).parent().unwrap()).unwrap();
        std::fs::write(source.join(path), content).unwrap();
    }
    for (roots, copied) in [
        (json!(["."]), "visible"),
        (json!(["worktree"]), "worktree/inner"),
    ] {
        let target = tempfile::tempdir().unwrap();
        let resources = tempfile::tempdir().unwrap();
        let mut config = config_value();
        config["workspace_paths"] = roots.clone();
        let selection = super::fs::prepare_command(
            &super::fs::open_directory(&source).unwrap(),
            &super::fs::open_directory(target.path()).unwrap(),
            &super::fs::open_directory(resources.path()).unwrap(),
            &serde_json::from_value(config).unwrap(),
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        )
        .unwrap();
        assert!(target.path().join(copied).is_file(), "{roots}");
        if roots != json!(["."]) {
            continue;
        }
        assert_eq!(selection.summary.pruned_directories, 2);
        assert!(!target.path().join("worktree").exists() && !target.path().join("repo").exists());
        let before = super::fs::command_snapshot(target.path()).unwrap();
        std::fs::create_dir(target.path().join("worktree")).unwrap();
        std::fs::write(target.path().join("worktree/generated"), b"output").unwrap();
        std::fs::write(target.path().join("fresh"), b"output").unwrap();
        let (changes, summary) = super::fs::selected_command_changeset(
            target.path(),
            &before,
            &selection,
            std::time::Instant::now() + super::selection::TEST_RUNTIME,
        )
        .unwrap();
        assert_eq!(summary.discarded_outputs, 1);
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "fresh");
    }
}

use super::config::Config;
use super::skills::parse_json;
use super::types::*;
use serde_json::{Value, json};

fn config_value() -> Value {
    json!({"version":1,"workspace_paths":["src","Cargo.toml"],"write":false,"commands":[],"skills":[],"mcp":[]})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn checked_tool_files_reject_links_traversal_and_special_objects() {
    use nix::{sys::stat::Mode, unistd::mkfifo};
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("project");
    std::fs::create_dir_all(root_path.join("nested")).unwrap();
    std::fs::write(root_path.join("nested/file"), b"admitted").unwrap();
    let root = super::fs::open_directory(&root_path).unwrap();
    std::os::unix::fs::symlink("nested", root_path.join("linked")).unwrap();
    std::os::unix::fs::symlink("file", root_path.join("nested/link")).unwrap();
    std::fs::hard_link(root_path.join("nested/file"), root_path.join("hardlink")).unwrap();
    mkfifo(&root_path.join("fifo"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    for path in [
        "linked/file",
        "nested/link",
        "hardlink",
        "fifo",
        "../outside",
        "nested/../nested/file",
        "/etc/passwd",
    ] {
        assert!(
            super::fs::open_file(&root, path)
                .and_then(|file| super::fs::read_regular(file, 32))
                .is_err(),
            "hostile file case {path}"
        );
    }
    std::fs::remove_file(root_path.join("hardlink")).unwrap();
    assert_eq!(
        super::fs::read_regular(super::fs::open_file(&root, "nested/file").unwrap(), 32).unwrap(),
        b"admitted"
    );
    assert!(
        super::fs::open_file(&root, ".")
            .unwrap()
            .metadata()
            .unwrap()
            .is_dir()
    );
    let config: Config = serde_json::from_value(json!({
        "version":1,"workspace_paths":["."],"write":true,
        "commands":[],"skills":[],"mcp":[]
    }))
    .unwrap();
    let write = ToolCall::Write {
        path: "nested/created".into(),
        expected_digest: None,
        content: "created".into(),
    };
    let result = super::fs::native(&root_path, &write, &config).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({"sha256":hex_digest(b"created"),"bytes":7})
    );
    assert!(super::fs::native(&root_path, &write, &config).is_err());
    let original = super::fs::open_file(&root, "nested/file").unwrap();
    let edit = ToolCall::Edit {
        path: "nested/file".into(),
        expected_digest: hex_digest(b"admitted"),
        old: "admitted".into(),
        new: "replaced".into(),
    };
    super::fs::native(&root_path, &edit, &config).unwrap();
    let mut old_bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(original, 32), &mut old_bytes).unwrap();
    assert_eq!(
        old_bytes, b"admitted",
        "atomic publication preserves the opened original inode"
    );
    assert_eq!(
        std::fs::read(root_path.join("nested/file")).unwrap(),
        b"replaced"
    );
    assert!(
        super::fs::native(&root_path, &edit, &config).is_err(),
        "stale digest cannot repeat a mutation"
    );
    for path in ["linked/file", "nested/link", "fifo", "../outside"] {
        let denied = ToolCall::Write {
            path: path.into(),
            expected_digest: Some(hex_digest(b"replaced")),
            content: "not admitted".into(),
        };
        assert!(
            super::fs::native(&root_path, &denied, &config).is_err(),
            "hostile mutation case {path}"
        );
    }
    let mut pinned = super::config::Program {
        name: "sh".into(),
        executable: std::fs::canonicalize(if cfg!(target_os = "macos") {
            "/bin/sh"
        } else {
            "/usr/bin/sh"
        })
        .unwrap()
        .to_str()
        .unwrap()
        .into(),
        sha256: String::new(),
        interpreter: true,
        inputs: Vec::new(),
    };
    let bytes = super::fs::read_regular(
        super::fs::open_absolute(&pinned.executable).unwrap(),
        MAX_SNAPSHOT_BYTES,
    )
    .unwrap();
    pinned.sha256 = hex_digest(&bytes);
    pinned.verify().unwrap();
    pinned.sha256 = "0".repeat(64);
    assert!(pinned.verify().is_err(), "changed executable pin rejects");

    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(root_path.join("empty")).unwrap();
    std::fs::write(root_path.join("script"), b"synthetic executable data").unwrap();
    std::fs::set_permissions(
        root_path.join("script"),
        std::fs::Permissions::from_mode(0o6755),
    )
    .unwrap();
    let input = temp.path().join("runtime-input");
    std::fs::write(&input, b"pinned input").unwrap();
    pinned.sha256 = hex_digest(&bytes);
    pinned.inputs.push(super::config::ProgramInput {
        path: input.to_str().unwrap().into(),
        destination: "data".into(),
        sha256: hex_digest(b"pinned input"),
    });
    let copied = temp.path().join("copied-program");
    std::fs::write(&copied, &bytes).unwrap();
    let copied_program = super::config::Program {
        name: "copied".into(),
        executable: copied.to_str().unwrap().into(),
        sha256: hex_digest(&bytes),
        interpreter: true,
        inputs: Vec::new(),
    };
    let snapshot_config = Config {
        version: 1,
        workspace_paths: vec!["nested/file".into(), "empty".into(), "script".into()],
        write: true,
        commands: vec![pinned, copied_program],
        skills: Vec::new(),
        mcp: Vec::new(),
        runtime_grants: Vec::new(),
    };
    for (case, drift) in [("fresh", false), ("changed-input", true)] {
        let target = temp.path().join(case);
        std::fs::create_dir_all(target.join("project")).unwrap();
        std::fs::create_dir(target.join("runtime")).unwrap();
        let project = super::fs::open_directory(&target.join("project")).unwrap();
        let resources = super::fs::open_directory(&target.join("runtime")).unwrap();
        if drift {
            std::fs::write(&input, b"changed after admission").unwrap();
        }
        let result = super::fs::snapshot_into(&root, &snapshot_config, Some(&project), &resources);
        if drift {
            assert!(
                matches!(result, Err(super::ToolError::ChangedInput)),
                "{case}"
            );
            continue;
        }
        result.unwrap();
        assert_eq!(
            std::fs::read(target.join("project/nested/file")).unwrap(),
            b"replaced"
        );
        assert!(target.join("project/empty").is_dir());
        assert_eq!(
            std::fs::metadata(target.join("project/script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert!(!target.join("project/linked").exists());
        assert!(!target.join("project/fifo").exists());
        if cfg!(target_os = "macos") {
            assert!(!target.join("runtime/programs/sh").exists());
        } else {
            assert_eq!(
                std::fs::read(target.join("runtime/programs/sh")).unwrap(),
                bytes
            );
        }
        assert_eq!(
            std::fs::read(target.join("runtime/programs/copied")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read(target.join("runtime/inputs/sh/data")).unwrap(),
            b"pinned input"
        );
        std::fs::write(target.join("project/nested/file"), b"disposable").unwrap();
        assert_eq!(
            std::fs::read(root_path.join("nested/file")).unwrap(),
            b"replaced"
        );
    }

    use std::os::unix::fs::MetadataExt;
    let host_root = super::fs::open_directory(std::path::Path::new("/")).unwrap();
    assert_ne!(
        std::fs::metadata("/").unwrap().dev(),
        std::fs::metadata("/dev").unwrap().dev(),
        "the native mount fixture must cross devices"
    );
    assert!(
        super::fs::open_file(&host_root, "dev").is_err(),
        "the handle traversal shared by scans and publication rejects mount crossings"
    );

    let workspace = temp.path().join("changes-workspace");
    let command = temp.path().join("changes-command");
    for directory in [&workspace, &command] {
        std::fs::create_dir(directory).unwrap();
        std::fs::create_dir(directory.join("empty")).unwrap();
        for (path, bytes, mode) in [
            ("binary", b"\0\xff\x01".as_slice(), 0o600),
            ("modified", b"original".as_slice(), 0o640),
            ("mode-off", b"off".as_slice(), 0o750),
            ("mode-on", b"on".as_slice(), 0o640),
            ("removed", b"removed".as_slice(), 0o600),
            ("unchanged", b"unchanged".as_slice(), 0o600),
        ] {
            std::fs::write(directory.join(path), bytes).unwrap();
            std::fs::set_permissions(directory.join(path), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
    }
    let modified = std::fs::File::open(workspace.join("modified")).unwrap();
    let original_inode = modified.metadata().unwrap().ino();
    let removed = std::fs::File::open(workspace.join("removed")).unwrap();
    let before = super::fs::command_snapshot(&command).unwrap();
    std::fs::write(command.join("binary"), b"\xff\0\x02").unwrap();
    std::fs::write(command.join("modified"), b"formatted").unwrap();
    std::fs::remove_file(command.join("removed")).unwrap();
    std::fs::remove_dir(command.join("empty")).unwrap();
    std::fs::create_dir_all(command.join("new/nested")).unwrap();
    std::fs::create_dir(command.join("unpublished-empty")).unwrap();
    std::fs::write(command.join("new/nested/first"), b"created").unwrap();
    std::fs::write(command.join("new/nested/second"), b"\0\xff").unwrap();
    std::fs::set_permissions(
        command.join("mode-off"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::set_permissions(
        command.join("mode-on"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    let changeset = super::fs::command_changeset(&command, &before).unwrap();
    let version = |bytes: &[u8], executable| Some(FileVersion::from_bytes(bytes, executable));
    let expected = CommandChangeset {
        files: [
            (
                "binary",
                version(b"\0\xff\x01", false),
                version(b"\xff\0\x02", false),
            ),
            ("mode-off", version(b"off", true), version(b"off", false)),
            ("mode-on", version(b"on", false), version(b"on", true)),
            (
                "modified",
                version(b"original", false),
                version(b"formatted", false),
            ),
            ("new/nested/first", None, version(b"created", false)),
            ("new/nested/second", None, version(b"\0\xff", false)),
            ("removed", version(b"removed", false), None),
        ]
        .into_iter()
        .map(|(path, before, after)| FileChange {
            path: path.into(),
            before,
            after,
        })
        .collect(),
    };
    assert_eq!(
        changeset, expected,
        "the command receipt retains complete sorted deltas"
    );
    let result = super::fs::native(
        &workspace,
        &ToolCall::ApplyChangeset {
            source_intent_digest: [1; 32],
            changeset,
        },
        &config,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({"applied":7})
    );
    for (path, bytes, mode) in [
        ("binary", b"\xff\0\x02".as_slice(), 0o600),
        ("modified", b"formatted".as_slice(), 0o640),
        ("mode-off", b"off".as_slice(), 0o640),
        ("mode-on", b"on".as_slice(), 0o740),
        ("new/nested/first", b"created".as_slice(), 0o600),
        ("new/nested/second", b"\0\xff".as_slice(), 0o600),
        ("unchanged", b"unchanged".as_slice(), 0o600),
    ] {
        assert_eq!(
            std::fs::read(workspace.join(path)).unwrap(),
            bytes,
            "published {path}"
        );
        assert_eq!(
            std::fs::metadata(workspace.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode,
            "published mode {path}"
        );
    }
    assert!(
        workspace.join("empty").is_dir(),
        "empty-directory removal is not integrated"
    );
    assert!(!workspace.join("unpublished-empty").exists());
    assert!(!workspace.join("removed").exists());
    assert_ne!(
        std::fs::metadata(workspace.join("modified")).unwrap().ino(),
        original_inode
    );
    for (file, expected) in [
        (modified, b"original".as_slice()),
        (removed, b"removed".as_slice()),
    ] {
        let mut retained = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(file, 32), &mut retained).unwrap();
        assert_eq!(
            retained, expected,
            "publication never truncates an admitted inode"
        );
    }
    let next = temp.path().join("next-command");
    std::fs::create_dir_all(next.join("project")).unwrap();
    std::fs::create_dir(next.join("resources")).unwrap();
    let next_project = super::fs::open_directory(&next.join("project")).unwrap();
    let next_resources = super::fs::open_directory(&next.join("resources")).unwrap();
    super::fs::snapshot_into(
        &super::fs::open_directory(&workspace).unwrap(),
        &config,
        Some(&next_project),
        &next_resources,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(next.join("project/modified")).unwrap(),
        b"formatted"
    );
    assert_eq!(
        std::fs::read(next.join("project/new/nested/second")).unwrap(),
        b"\0\xff"
    );
    assert!(!next.join("project/removed").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn command_changesets_reject_invalid_snapshots_and_batches_before_publication() {
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for case in [
        "changed-bytes",
        "changed-executable",
        "create-conflict",
        "missing-before",
        "symlink",
        "symlink-parent",
        "hardlink",
        "fifo",
        "set-id-file",
        "set-id-parent",
        "directory-target",
        "file-parent",
        "excluded",
        "write-denied",
        "grant-denied",
        "malformed-base64",
        "noncanonical-base64",
        "wrong-digest",
        "malformed-digest",
        "unchanged-delta",
        "absent-delta",
        "reordered",
        "duplicate",
        "path-collision",
        "too-many-files",
        "too-many-bytes",
        "too-deep",
        "oversized-record",
        "unbound-source",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("a-good"), b"stable").unwrap();
        std::fs::write(workspace.join("z-target"), b"old").unwrap();
        std::fs::set_permissions(
            workspace.join("z-target"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let sibling_inode = std::fs::metadata(workspace.join("a-good")).unwrap().ino();
        let mut config: Config = serde_json::from_value(json!({
            "version":1,"workspace_paths":["."],"write":true,
            "commands":[],"skills":[],"mcp":[]
        }))
        .unwrap();
        let mut changeset = CommandChangeset {
            files: vec![
                FileChange {
                    path: "a-good".into(),
                    before: Some(FileVersion::from_bytes(b"stable", false)),
                    after: Some(FileVersion::from_bytes(b"published", false)),
                },
                FileChange {
                    path: "z-target".into(),
                    before: Some(FileVersion::from_bytes(b"old", false)),
                    after: Some(FileVersion::from_bytes(b"new", false)),
                },
            ],
        };
        let mut source_intent_digest = [1; 32];
        match case {
            "changed-bytes" => std::fs::write(workspace.join("z-target"), b"concurrent").unwrap(),
            "changed-executable" => std::fs::set_permissions(
                workspace.join("z-target"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap(),
            "create-conflict" => changeset.files[1].before = None,
            "missing-before" => std::fs::remove_file(workspace.join("z-target")).unwrap(),
            "symlink" | "hardlink" | "fifo" | "directory-target" => {
                std::fs::remove_file(workspace.join("z-target")).unwrap();
                match case {
                    "symlink" => {
                        std::os::unix::fs::symlink("a-good", workspace.join("z-target")).unwrap()
                    }
                    "hardlink" => {
                        std::fs::hard_link(workspace.join("a-good"), workspace.join("z-target"))
                            .unwrap()
                    }
                    "fifo" => {
                        mkfifo(&workspace.join("z-target"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap()
                    }
                    _ => std::fs::create_dir(workspace.join("z-target")).unwrap(),
                }
            }
            "symlink-parent" | "set-id-parent" => {
                std::fs::remove_file(workspace.join("z-target")).unwrap();
                std::fs::create_dir(workspace.join("directory")).unwrap();
                std::fs::write(workspace.join("directory/file"), b"old").unwrap();
                if case == "symlink-parent" {
                    std::os::unix::fs::symlink("directory", workspace.join("z-target")).unwrap();
                } else {
                    std::fs::rename(workspace.join("directory"), workspace.join("z-target"))
                        .unwrap();
                    std::fs::set_permissions(
                        workspace.join("z-target"),
                        std::fs::Permissions::from_mode(0o4700),
                    )
                    .unwrap();
                }
                changeset.files[1].path = "z-target/file".into();
            }
            "set-id-file" => std::fs::set_permissions(
                workspace.join("z-target"),
                std::fs::Permissions::from_mode(0o4600),
            )
            .unwrap(),
            "file-parent" => changeset.files[1].path = "z-target/file".into(),
            "excluded" => changeset.files[1].path = "z-target/.git/config".into(),
            "write-denied" => config.write = false,
            "grant-denied" => config.workspace_paths = vec!["a-good".into()],
            "malformed-base64" => changeset.files[1].after.as_mut().unwrap().content = "%%".into(),
            "noncanonical-base64" => {
                changeset.files[1].after.as_mut().unwrap().content = "bmV3\n".into()
            }
            "wrong-digest" => {
                changeset.files[1].after.as_mut().unwrap().sha256 = hex_digest(b"other")
            }
            "malformed-digest" => {
                changeset.files[1].after.as_mut().unwrap().sha256 = "F".repeat(64)
            }
            "unchanged-delta" => changeset.files[1].after = changeset.files[1].before.clone(),
            "absent-delta" => {
                changeset.files[1].before = None;
                changeset.files[1].after = None;
            }
            "reordered" => changeset.files.reverse(),
            "duplicate" => changeset.files.push(changeset.files[1].clone()),
            "path-collision" => changeset.files.push(FileChange {
                path: "z-target/child".into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"child", false)),
            }),
            "too-many-files" | "oversized-record" => {
                let count = if case == "too-many-files" {
                    MAX_CHANGESET_FILES
                } else {
                    MAX_CHANGESET_FILES - 1
                };
                for index in 0..count {
                    changeset.files.insert(
                        index + 1,
                        FileChange {
                            path: if case == "too-many-files" {
                                format!("y-{index:02}")
                            } else {
                                format!("y-{index:02}-{}", "x".repeat(480))
                            },
                            before: None,
                            after: Some(FileVersion::from_bytes(b"", false)),
                        },
                    );
                }
                if case == "oversized-record" {
                    changeset.files.pop();
                }
            }
            "too-many-bytes" => {
                changeset.files[1].after = Some(FileVersion::from_bytes(
                    &vec![0; MAX_CHANGESET_BYTES],
                    false,
                ))
            }
            "too-deep" => changeset.files[1].path = format!("z-target/{}", vec!["d"; 16].join("/")),
            "unbound-source" => source_intent_digest = [0; 32],
            _ => unreachable!(),
        }
        if matches!(case, "set-id-file" | "set-id-parent") {
            assert_ne!(
                std::fs::metadata(workspace.join("z-target"))
                    .unwrap()
                    .mode()
                    & 0o6000,
                0,
                "the native fixture must retain set-ID bits: {case}"
            );
        }
        let result = super::fs::native(
            &workspace,
            &ToolCall::ApplyChangeset {
                source_intent_digest,
                changeset,
            },
            &config,
        );
        assert!(result.is_err(), "batch rejection case {case}");
        assert!(
            !matches!(result, Err(super::ToolError::Uncertain)),
            "preflight must reject before effects: {case}"
        );
        assert_eq!(
            std::fs::read(workspace.join("a-good")).unwrap(),
            b"stable",
            "unchanged sibling: {case}"
        );
        assert_eq!(
            std::fs::metadata(workspace.join("a-good")).unwrap().ino(),
            sibling_inode,
            "unpublished sibling inode: {case}"
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("partial-publication");
    std::fs::create_dir_all(workspace.join("a")).unwrap();
    std::fs::create_dir(workspace.join("z")).unwrap();
    std::fs::write(workspace.join("a/new"), b"original").unwrap();
    let original = std::fs::File::open(workspace.join("a/new")).unwrap();
    let original_inode = original.metadata().unwrap().ino();
    let blocked_parent = workspace.join("z");
    std::fs::set_permissions(&blocked_parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let parent_metadata = std::fs::metadata(&blocked_parent).unwrap();
    let effective_uid = rustix::process::geteuid().as_raw();
    assert_ne!(
        effective_uid, 0,
        "the fixture requires ordinary-user write denial"
    );
    assert_eq!(
        parent_metadata.uid(),
        effective_uid,
        "the blocked parent is fixture-owned"
    );
    assert_eq!(
        parent_metadata.mode() & 0o7777,
        0o500,
        "the fixture must retain read-only parent permissions"
    );
    let config: Config = serde_json::from_value(json!({
        "version":1,"workspace_paths":["."],"write":true,
        "commands":[],"skills":[],"mcp":[]
    }))
    .unwrap();
    let result = super::fs::native(
        &workspace,
        &ToolCall::ApplyChangeset {
            source_intent_digest: [1; 32],
            changeset: CommandChangeset {
                files: vec![
                    FileChange {
                        path: "a/new".into(),
                        before: Some(FileVersion::from_bytes(b"original", false)),
                        after: Some(FileVersion::from_bytes(b"published", false)),
                    },
                    FileChange {
                        path: "z/new".into(),
                        before: None,
                        after: Some(FileVersion::from_bytes(b"blocked", false)),
                    },
                ],
            },
        },
        &config,
    );
    let retained_parent_mode = std::fs::metadata(&blocked_parent).unwrap().mode() & 0o7777;
    std::fs::set_permissions(&blocked_parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(result, Err(super::ToolError::Uncertain)),
        "a failure after publication cannot become an ordinary retryable error"
    );
    assert_eq!(
        retained_parent_mode, 0o500,
        "publication does not widen parent permissions"
    );
    assert_eq!(
        std::fs::read(workspace.join("a/new")).unwrap(),
        b"published",
        "an uncertain batch does not roll back its published prefix"
    );
    let published = std::fs::metadata(workspace.join("a/new")).unwrap();
    assert!(published.is_file());
    assert_eq!(published.nlink(), 1);
    assert_ne!(
        published.ino(),
        original_inode,
        "the published prefix uses a fresh inode"
    );
    assert!(
        !workspace.join("z/new").exists(),
        "the later file was not published"
    );
    let mut retained = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(original, 32), &mut retained).unwrap();
    assert_eq!(
        retained, b"original",
        "partial publication does not truncate the original inode"
    );

    for case in [
        "symlink",
        "symlink-parent",
        "hardlink",
        "fifo",
        "set-id-file",
        "set-id-directory",
        "set-id-root",
        "excluded",
        "invalid-name",
        "file-to-directory",
        "directory-to-file",
        "replaced-root",
        "file-overflow",
        "snapshot-bytes-overflow",
        "snapshot-count-overflow",
        "changeset-count-limit",
        "changeset-count-overflow",
        "changeset-byte-limit",
        "changeset-byte-overflow",
        "depth-limit",
        "depth-overflow",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let command = temp.path().join("command");
        std::fs::create_dir(&command).unwrap();
        std::fs::write(command.join("unchanged"), b"seed").unwrap();
        if case == "file-to-directory" {
            std::fs::write(command.join("target"), b"old").unwrap();
        }
        if case == "directory-to-file" {
            std::fs::create_dir(command.join("target")).unwrap();
        }
        let before = super::fs::command_snapshot(&command).unwrap();
        match case {
            "symlink" => std::os::unix::fs::symlink("unchanged", command.join("target")).unwrap(),
            "symlink-parent" => {
                std::fs::create_dir(command.join("directory")).unwrap();
                std::fs::write(command.join("directory/file"), b"child").unwrap();
                std::os::unix::fs::symlink("directory", command.join("target")).unwrap();
            }
            "hardlink" => {
                std::fs::hard_link(command.join("unchanged"), command.join("target")).unwrap()
            }
            "fifo" => mkfifo(&command.join("target"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap(),
            "set-id-file" | "set-id-directory" | "set-id-root" => {
                let target = if case == "set-id-root" {
                    command.clone()
                } else {
                    command.join("target")
                };
                if case == "set-id-file" {
                    std::fs::write(&target, b"child").unwrap();
                }
                if case == "set-id-directory" {
                    std::fs::create_dir(&target).unwrap();
                }
                std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o4700)).unwrap();
                let target = if case == "set-id-root" {
                    &command
                } else {
                    &command.join("target")
                };
                assert_ne!(
                    std::fs::metadata(target).unwrap().mode() & 0o6000,
                    0,
                    "the native fixture must retain set-ID bits: {case}"
                );
            }
            "excluded" => {
                std::fs::create_dir(command.join(".git")).unwrap();
                std::fs::write(command.join(".git/config"), b"excluded").unwrap();
            }
            "invalid-name" => std::fs::write(command.join("line\nbreak"), b"invalid").unwrap(),
            "file-to-directory" => {
                std::fs::remove_file(command.join("target")).unwrap();
                std::fs::create_dir(command.join("target")).unwrap();
            }
            "directory-to-file" => {
                std::fs::remove_dir(command.join("target")).unwrap();
                std::fs::write(command.join("target"), b"new").unwrap();
            }
            "replaced-root" => {
                std::fs::rename(&command, temp.path().join("replaced")).unwrap();
                std::fs::create_dir(&command).unwrap();
                std::fs::write(command.join("unchanged"), b"seed").unwrap();
            }
            "file-overflow" => {
                std::fs::write(command.join("target"), vec![0; MAX_FILE_BYTES + 1]).unwrap()
            }
            "snapshot-bytes-overflow" => {
                for index in 0..MAX_SNAPSHOT_BYTES / MAX_FILE_BYTES {
                    std::fs::write(
                        command.join(format!("file-{index:02}")),
                        vec![0; MAX_FILE_BYTES],
                    )
                    .unwrap();
                }
            }
            "snapshot-count-overflow" | "changeset-count-limit" | "changeset-count-overflow" => {
                let count = match case {
                    "snapshot-count-overflow" => MAX_SNAPSHOT_FILES,
                    "changeset-count-limit" => MAX_CHANGESET_FILES,
                    _ => MAX_CHANGESET_FILES + 1,
                };
                for index in 0..count {
                    std::fs::write(command.join(format!("file-{index:04}")), b"").unwrap();
                }
            }
            "changeset-byte-limit" | "changeset-byte-overflow" => {
                let count = MAX_CHANGESET_BYTES + usize::from(case == "changeset-byte-overflow");
                std::fs::write(command.join("target"), vec![0; count]).unwrap();
            }
            "depth-limit" | "depth-overflow" => {
                let path = vec!["d"; if case == "depth-limit" { 15 } else { 16 }].join("/");
                std::fs::create_dir_all(command.join(&path)).unwrap();
                std::fs::write(command.join(path).join("target"), b"child").unwrap();
            }
            _ => unreachable!(),
        }
        let result = super::fs::command_changeset(&command, &before);
        match case {
            "changeset-count-limit" => {
                assert_eq!(result.unwrap().files.len(), MAX_CHANGESET_FILES, "{case}")
            }
            "changeset-byte-limit" | "depth-limit" => {
                assert_eq!(result.unwrap().files.len(), 1, "{case}")
            }
            "file-overflow"
            | "snapshot-bytes-overflow"
            | "snapshot-count-overflow"
            | "changeset-count-overflow"
            | "changeset-byte-overflow"
            | "depth-overflow" => assert!(
                matches!(result, Err(super::ToolError::Limit)),
                "snapshot bound: {case}"
            ),
            "replaced-root" => assert!(
                matches!(result, Err(super::ToolError::ChangedInput)),
                "snapshot identity: {case}"
            ),
            _ => assert!(
                matches!(result, Err(super::ToolError::Path)),
                "snapshot object: {case}"
            ),
        }
    }
}

#[cfg(unix)]
#[test]
fn project_skill_discovery_preserves_grants_and_reports_missing_installation() {
    for layout in [
        "absent",
        "lock",
        "empty",
        "installed",
        "many-installed",
        "linked-directory",
        "linked-file",
        "linked-resource",
        "oversized",
        "too-many",
        "too-many-resources",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        for ignored in [
            "agents/skills/ignored",
            ".claude/skills/ignored",
            ".codex/skills/ignored",
        ] {
            std::fs::create_dir_all(workspace.join(ignored)).unwrap();
            std::fs::write(workspace.join(ignored).join("SKILL.md"), b"ignored").unwrap();
        }
        if layout != "absent" {
            std::fs::write(workspace.join("skills-lock.json"), b"unparsed lock data").unwrap();
        }
        let folder = workspace.join(".agents/skills");
        if !matches!(layout, "absent" | "lock") {
            std::fs::create_dir_all(&folder).unwrap();
        }
        if !matches!(layout, "absent" | "lock" | "empty") {
            let count = if layout == "too-many" {
                MAX_SKILLS + 1
            } else if layout == "many-installed" {
                MAX_SKILLS
            } else {
                1
            };
            for index in 0..count {
                let name = format!("skill-{index:02}");
                let dir = folder.join(&name);
                std::fs::create_dir_all(dir.join("references")).unwrap();
                let guidance =
                    format!("---\nname: {name}\ndescription: Useful project guidance\n---\nBody\n");
                std::fs::write(dir.join("SKILL.md"), guidance).unwrap();
                std::fs::write(dir.join("references/note.txt"), b"A pinned resource").unwrap();
                if layout == "linked-directory" {
                    std::os::unix::fs::symlink(&dir, folder.join("linked")).unwrap();
                } else if layout == "linked-file" {
                    std::fs::create_dir(folder.join("linked")).unwrap();
                    std::os::unix::fs::symlink(
                        dir.join("SKILL.md"),
                        folder.join("linked/SKILL.md"),
                    )
                    .unwrap();
                } else if layout == "linked-resource" {
                    std::os::unix::fs::symlink(
                        dir.join("SKILL.md"),
                        dir.join("references/linked.txt"),
                    )
                    .unwrap();
                } else if layout == "oversized" {
                    std::fs::write(dir.join("SKILL.md"), vec![b'x'; MAX_TOOL_RESULT_BYTES + 1])
                        .unwrap();
                } else if layout == "too-many-resources" {
                    for resource in 0..32 {
                        std::fs::write(dir.join(format!("references/{resource}.txt")), b"extra")
                            .unwrap();
                    }
                }
            }
        }
        let discovery = super::project_skills(&workspace);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if !matches!(layout, "absent" | "lock") {
            assert!(
                matches!(&discovery, Err(super::ToolError::ProtectionUnavailable)),
                "{layout}: {:?}",
                discovery.as_ref().err()
            );
            let mut value = config_value();
            value["workspace_paths"] = json!(["."]);
            let mut config: Config = serde_json::from_value(value).unwrap();
            assert!(
                matches!(
                    config.discover_skills(&workspace),
                    Err(super::ToolError::ProtectionUnavailable)
                ),
                "{layout}"
            );
            continue;
        }
        if matches!(
            layout,
            "linked-directory" | "linked-file" | "oversized" | "too-many"
        ) {
            assert!(discovery.is_err(), "{layout}");
            continue;
        }
        let discovery = discovery.unwrap();
        assert_eq!(discovery.installation_missing, layout == "lock", "{layout}");
        let mut value = config_value();
        value["workspace_paths"] = json!(["."]);
        let mut config: Config = serde_json::from_value(value).unwrap();
        let pinned = config.discover_skills(&workspace);
        if matches!(layout, "linked-resource" | "too-many-resources") {
            assert!(pinned.is_err(), "{layout}");
            continue;
        }
        pinned.unwrap();
        let expected = if layout == "many-installed" {
            MAX_SKILLS
        } else {
            usize::from(layout == "installed")
        };
        assert_eq!(config.skills.len(), expected, "{layout}");
        if matches!(layout, "installed" | "many-installed") {
            assert_eq!(discovery.names.len(), expected);
            assert_eq!(discovery.names[0], "skill-00");
            assert_eq!(
                config.skills[0].files["references/note.txt"],
                hex_digest(b"A pinned resource")
            );
            let read = ToolCall::Skill {
                name: "skill-00".into(),
                resource: Some("references/note.txt".into()),
            };
            assert!(config.allows(&read));
            config.skills[0].description = "Private grant takes precedence".into();
            config.discover_skills(&workspace).unwrap();
            assert_eq!(config.skills.len(), expected);
            assert_eq!(
                config.skills[0].description,
                "Private grant takes precedence"
            );
            let mut narrow: Config = serde_json::from_value(config_value()).unwrap();
            narrow.discover_skills(&workspace).unwrap();
            assert!(narrow.skills.is_empty());
            assert!(!narrow.allows(&read));
            let catalog = json!({"skills":config.skills.iter().map(|skill| json!({"name":skill.name,"description":"x".repeat(150)})).collect::<Vec<_>>()}).to_string();
            if layout == "many-installed" {
                assert!(catalog.len() > 8 * 1024);
            }
            let context = ToolContext {
                catalog,
                observations: Vec::new(),
            };
            assert!(context.model_input()["catalog"].is_object());
            let branch = context.outcome_branch();
            let skill = branch["properties"]["call"]["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|call| call["properties"]["operation"]["enum"][0] == "skill")
                .unwrap();
            assert_eq!(
                skill["properties"]["name"]["enum"]
                    .as_array()
                    .unwrap()
                    .len(),
                expected
            );
        }
    }
}

#[test]
fn tool_admission_corpus_is_closed_and_restrict_only() {
    let value = config_value();
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();
    assert!(config.receipt().valid());
    for roots in [
        json!([]),
        json!(["src", "src/child"]),
        json!(["src", "src"]),
        json!([".", "src"]),
        json!(["../outside"]),
        json!(["src/.env"]),
        json!(["/absolute"]),
    ] {
        let mut row = value.clone();
        row["workspace_paths"] = roots;
        assert!(
            serde_json::from_value::<Config>(row)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut root_value = value.clone();
    root_value["workspace_paths"] = json!(["."]);
    let root_config: Config = serde_json::from_value(root_value.clone()).unwrap();
    root_config.validate().unwrap();
    assert!(root_config.allows(&ToolCall::List { path: ".".into() }));
    assert!(!root_config.allows(&ToolCall::Mkdir { path: ".".into() }));
    for (path, expected) in [
        ("README.md", true),
        ("nested/source.rs", true),
        (".git/config", false),
        (".env.local", false),
        ("../outside", false),
    ] {
        assert_eq!(
            root_config.allows(&ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1
            }),
            expected,
            "root grant: {path}"
        );
    }
    root_value["version"] = json!(2);
    assert!(
        serde_json::from_value::<Config>(root_value)
            .unwrap()
            .validate()
            .is_err()
    );
    let duplicate = br#"{"version":1,"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}"#;
    assert!(parse_json(duplicate, 64 * 1024).is_err());
    let mut unknown = value.clone();
    unknown["network"] = json!(true);
    assert!(serde_json::from_value::<Config>(unknown).is_err());
    for (path, expected) in [
        ("src/main.rs", true),
        ("src2/main.rs", false),
        ("src/../omitted", false),
        ("src/.git/config", false),
        ("src/.env.local", false),
        ("src/line\nbreak", false),
    ] {
        assert_eq!(
            config.allows(&ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1
            }),
            expected,
            "path case"
        );
    }
    assert!(!config.allows(&ToolCall::Write {
        path: "src/new".into(),
        expected_digest: None,
        content: "new".into()
    }));
    assert!(!config.allows(&ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "".into()
    }));
    assert!(!config.allows(&ToolCall::Skill {
        name: "unconfigured".into(),
        resource: None
    }));
    assert!(!config.allows(&ToolCall::McpList {
        server: "unconfigured".into()
    }));
    let mut duplicate_names = value;
    duplicate_names["mcp"] = json!([{"name":"example","program":{"name":"python","executable":"/usr/bin/python","sha256":"0".repeat(64),"interpreter":true,"inputs":[]},"args":[],"tools":["echo","echo"]}]);
    assert!(
        serde_json::from_value::<Config>(duplicate_names)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn host_runtime_roots_can_only_come_from_remembered_folder_trust() {
    use crate::store::StateRoot;
    use crate::tools::toolchains::{RuntimeKind, discover};
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let node = home.join(".nvm/versions/node/v22.3.0");
    std::fs::create_dir_all(node.join("bin")).unwrap();
    std::fs::write(node.join("bin/node"), b"synthetic executable").unwrap();
    std::fs::set_permissions(
        node.join("bin/node"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let grant = discover(&home)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.grant.kind == RuntimeKind::Node)
        .unwrap()
        .grant;
    let mut value = config_value();
    value["runtime_grants"] = json!([grant]);
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();

    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let path = state.path().join("tools.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Config::load(&state).is_err());
}

#[test]
fn typed_tool_schema_and_receipts_have_exact_bounds() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../../tests/fixtures/tool-outcomes.json")).unwrap();
    assert_eq!(rows.len(), 10);
    let branch = outcome_branch();
    let schema = jsonschema::options().offline().build(&branch).unwrap();
    for row in &rows {
        assert!(schema.is_valid(&row["outcome"]));
        let call: ToolCall = serde_json::from_value(row["outcome"]["call"].clone()).unwrap();
        assert!(call.valid());
        let mut unknown = row["outcome"]["call"].clone();
        unknown["network"] = json!(true);
        assert!(serde_json::from_value::<ToolCall>(unknown).is_err());
    }
    for (index, key) in [(4, "expected_digest"), (7, "resource")] {
        let mut missing = rows[index]["outcome"]["call"].clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ToolCall>(missing).is_err(),
            "nullable fields are still required"
        );
    }
    for call in [
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: 0,
            limit: 4097,
        },
        ToolCall::Search {
            path: "src".into(),
            query: "x".repeat(513),
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "x".repeat(8193),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".into(); 65],
            cwd: "".into(),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["nul\0argument".into()],
            cwd: "".into(),
        },
    ] {
        assert!(!call.valid());
    }
    let intent = EffectIntent {
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::List { path: "src".into() },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    assert!(intent.valid());
    let mut limits = json!({
        "result_bytes": MAX_TOOL_RESULT_BYTES,
        "runtime_ms": 60_000,
        "memory_bytes": MEMORY_BYTES,
        "max_processes": MAX_PROCESSES_OR_THREADS,
        "scratch_bytes": SCRATCH_BYTES,
        "network": "none"
    });
    assert!(serde_json::from_value::<ToolLimits>(limits.clone()).is_err());
    let mut native = intent.clone();
    native.limits.resource_profile = ToolResourceProfile::LinuxKernel;
    let linux_intent = native.digest();
    native.limits.resource_profile = ToolResourceProfile::MacosSupervised;
    for profile in [
        ToolResourceProfile::LinuxKernel,
        ToolResourceProfile::MacosSupervised,
    ] {
        limits["resource_profile"] = serde_json::to_value(profile).unwrap();
        let decoded: ToolLimits = serde_json::from_value(limits.clone()).unwrap();
        assert_eq!(decoded.resource_profile, profile);
        assert_eq!(serde_json::to_value(&decoded).unwrap(), limits);
    }
    assert!(native.valid());
    assert_ne!(
        native.digest(),
        linux_intent,
        "resource semantics bind the intent"
    );
    assert_eq!(
        serde_json::from_value::<EffectIntent>(serde_json::to_value(&native).unwrap()).unwrap(),
        native
    );
    let guard = GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    };
    let observation = ToolObservation {
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "observed".into(),
        guard: Some(guard),
    };
    assert!(observation.valid());
    let mut profile_mismatch = observation.clone();
    profile_mismatch
        .guard
        .as_mut()
        .unwrap()
        .limits
        .resource_profile =
        if intent.limits.resource_profile == ToolResourceProfile::MacosSupervised {
            ToolResourceProfile::LinuxKernel
        } else {
            ToolResourceProfile::MacosSupervised
        };
    assert!(
        !profile_mismatch.valid(),
        "receipt cannot relabel resource guarantees"
    );
    let mut missing = observation.clone();
    missing.guard = None;
    assert!(!missing.valid());
    let mut mismatched = observation.clone();
    mismatched.guard.as_mut().unwrap().limits.max_processes += 1;
    assert!(!mismatched.valid());
    let mut widened = intent.clone();
    widened.limits.runtime_ms += 1;
    assert!(!widened.valid());
    let mut reusable = intent;
    reusable.use_count = 2;
    assert!(!reusable.valid());
    let errors = vec![
        super::ToolError::MissingConfiguration,
        super::ToolError::Configuration,
        super::ToolError::ProtectionUnavailable,
        super::ToolError::ChangedInput,
        super::ToolError::Path,
        super::ToolError::Limit,
        super::ToolError::Conflict,
        super::ToolError::Operation,
        super::ToolError::Uncertain,
        super::ToolError::GuardRejected(super::guard::GuardStage::ResourceAttestation),
        super::ToolError::GuardRejected(super::guard::GuardStage::ProcessCleanup),
        super::ToolError::GuardRejected(super::guard::GuardStage::LauncherReap),
        super::ToolError::GuardRejected(super::guard::GuardStage::OutputEof),
        super::ToolError::GuardRejected(super::guard::GuardStage::NamespaceBootstrap),
        super::ToolError::GuardRejected(super::guard::GuardStage::NativeManager),
        super::ToolError::GuardRejected(super::guard::GuardStage::ControlProtocol),
    ];
    for (stage, label) in [
        ("admission", "admission"),
        ("workspace", "pinned input"),
        ("capabilities", "native protection"),
        ("handshake", "handshake"),
        ("payload", "execution"),
        ("receipt", "receipt"),
    ] {
        let error = super::ToolError::GuardBootstrap(
            serde_json::from_value(serde_json::json!({"stage": stage})).unwrap(),
        );
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let receipt = super::uncertain_observation(reserved, error);
        assert_eq!(
            receipt.output,
            format!(
                "Guard {label} unconfirmed; effects may have occurred. Do not retry automatically."
            )
        );
        assert!(receipt.valid());
    }
    for error in errors {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let uncertain = super::uncertain_observation(reserved, error);
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.guard.is_none());
        assert!(uncertain.valid(), "minimum-budget uncertainty receipt");
        assert!(uncertain.output.ends_with("Do not retry automatically."));
    }
    let mut oversized = observation;
    oversized.output = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
    assert!(!oversized.valid());
}

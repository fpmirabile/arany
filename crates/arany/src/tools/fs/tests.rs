use crate::tools::catalog::Site;
use crate::tools::config::Config;
use crate::tools::tests::{in_process_runtime, run_in_process};
use crate::tools::types::*;
use crate::validate::hex_digest;
use serde_json::{Value, json};

#[test]
fn multiedit_orders_and_bounds_the_whole_publication() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    let config: Config = serde_json::from_value(
        json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    )
    .unwrap();
    let edit = |old: &str, new: &str, replace_all| LiteralEdit {
        old_string: old.into(),
        new_string: new.into(),
        replace_all,
    };
    let call = |bytes: &[u8], edits| ToolCall::MultiEdit {
        file_path: "file".into(),
        expected_digest: Some(hex_digest(bytes)),
        edits,
    };
    for (name, original, edits, expected) in [
        (
            "dependent",
            "A",
            vec![edit("A", "B", false), edit("B", "C", false)],
            Some("C"),
        ),
        (
            "all literal",
            "aa aa",
            vec![edit("aa", "$1\\é", true)],
            Some("$1\\é $1\\é"),
        ),
        ("nonoverlap", "xxx", vec![edit("xx", "y", true)], Some("yx")),
        (
            "fidelity",
            "é\r\nquote ‘x’\r\n",
            vec![edit("é", "界", false), edit("‘x’", "", false)],
            Some("界\r\nquote \r\n"),
        ),
        (
            "late missing",
            "A",
            vec![edit("A", "B", false), edit("missing", "C", false)],
            None,
        ),
        (
            "late ambiguous",
            "A B",
            vec![edit("A", "B", false), edit("B", "C", false)],
            None,
        ),
        ("missing all", "A", vec![edit("B", "C", true)], None),
        (
            "unchanged final",
            "A",
            vec![edit("A", "B", false), edit("B", "A", false)],
            None,
        ),
    ] {
        std::fs::write(&file, original).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let opened = std::fs::File::open(&file).unwrap();
        let call = call(original.as_bytes(), edits);
        assert!(call.valid(), "{name}");
        let result = super::fs::native(temp.path(), &call, &config);
        if let Some(expected) = expected {
            let output: Value = serde_json::from_str(&result.unwrap()).unwrap();
            assert_eq!(
                output,
                json!({"before":{"sha256":hex_digest(original.as_bytes()),"bytes":original.len(),"executable":true},"sha256":hex_digest(expected.as_bytes()),"bytes":expected.len(),"executable":true}),
                "{name}"
            );
            assert_eq!(std::fs::read(&file).unwrap(), expected.as_bytes(), "{name}");
        } else {
            assert!(matches!(result, Err(super::ToolError::Conflict)), "{name}");
            assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes(), "{name}");
        }
        let mut original_inode = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(opened, MAX_FILE_BYTES as u64),
            &mut original_inode,
        )
        .unwrap();
        assert_eq!(
            original_inode,
            original.as_bytes(),
            "{name}: one fresh inode publication"
        );
    }
    let ordered: Vec<_> = (0..MAX_MULTI_EDITS)
        .map(|index| edit(&index.to_string(), &(index + 1).to_string(), false))
        .collect();
    std::fs::write(&file, "0").unwrap();
    super::fs::native(temp.path(), &call(b"0", ordered), &config).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"64");
    let original = "a".repeat(MAX_FILE_BYTES);
    std::fs::write(&file, &original).unwrap();
    for (edits, accepted) in [
        (vec![edit("a", "b", true)], true),
        (vec![edit("a", "aa", true), edit("aa", "b", true)], false),
    ] {
        std::fs::write(&file, &original).unwrap();
        let result = super::fs::native(temp.path(), &call(original.as_bytes(), edits), &config);
        assert_eq!(result.is_ok(), accepted);
        if !accepted {
            assert!(matches!(result, Err(super::ToolError::Limit)));
        }
    }
    std::fs::write(&file, &original).unwrap();
    let overflowing = call(original.as_bytes(), vec![edit("a", "aa", true)]);
    assert!(matches!(
        super::fs::native(temp.path(), &overflowing, &config),
        Err(super::ToolError::Limit)
    ));
    assert_eq!(std::fs::read(&file).unwrap(), original.as_bytes());
    for bytes in [vec![0xff], vec![b'a'; MAX_FILE_BYTES + 1]] {
        std::fs::write(&file, &bytes).unwrap();
        assert!(
            super::fs::native(
                temp.path(),
                &call(&bytes, vec![edit("a", "b", false)]),
                &config
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }
    std::fs::remove_file(&file).unwrap();
    assert!(
        super::fs::native(
            temp.path(),
            &call(b"", vec![edit("a", "b", false)]),
            &config
        )
        .is_err()
    );
    assert!(!file.exists());
    for (edits, accepted) in [
        (vec![], false),
        (vec![edit("a", "b", false); MAX_MULTI_EDITS], true),
        (vec![edit("a", "b", false); MAX_MULTI_EDITS + 1], false),
        (vec![edit("", "b", true)], false),
        (vec![edit("a", "a", false)], false),
        (
            vec![edit("é", &"x".repeat(MAX_EDIT_CONTENT_BYTES - 2), false)],
            true,
        ),
        (
            vec![edit("é", &"x".repeat(MAX_EDIT_CONTENT_BYTES - 1), false)],
            false,
        ),
        (
            vec![
                edit(
                    &"x".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                    &"y".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                    false,
                ),
                edit("a", "b", false),
            ],
            false,
        ),
    ] {
        assert_eq!(call(b"a", edits).valid(), accepted);
    }
    for path in ["/file", "../file", ".env", "nested/link"] {
        let mut value = serde_json::to_value(call(b"a", vec![edit("a", "b", false)])).unwrap();
        value["file_path"] = json!(path);
        let denied: ToolCall = serde_json::from_value(value).unwrap();
        assert!(
            !config.allows(&denied) || super::fs::native(temp.path(), &denied, &config).is_err()
        );
    }
}

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
        "workspace_paths":["."],"write":true,
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
        json!({"sha256":hex_digest(b"created"),"bytes":7,"executable":false,"before":null})
    );
    assert!(super::fs::native(&root_path, &write, &config).is_err());
    let deep = ToolCall::Write {
        path: "fresh/deeper/created".into(),
        expected_digest: None,
        content: "deep".into(),
    };
    let result = super::fs::native(&root_path, &deep, &config).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({"sha256":hex_digest(b"deep"),"bytes":4,"executable":false,"before":null})
    );
    assert_eq!(
        std::fs::read(root_path.join("fresh/deeper/created")).unwrap(),
        b"deep"
    );
    for path in ["linked/fresh/created", "nested/file/created"] {
        let refused = ToolCall::Write {
            path: path.into(),
            expected_digest: None,
            content: "not admitted".into(),
        };
        assert!(
            super::fs::native(&root_path, &refused, &config).is_err(),
            "missing parent beneath {path}"
        );
    }
    let original = super::fs::open_file(&root, "nested/file").unwrap();
    let edit = ToolCall::Edit {
        path: "nested/file".into(),
        expected_digest: Some(hex_digest(b"admitted")),
        old: "admitted".into(),
        new: "replaced".into(),
    };
    let output = super::fs::native(&root_path, &edit, &config).unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["before"]["sha256"], hex_digest(b"admitted"));
    assert_eq!(output["before"]["bytes"], 8);
    assert_eq!(output["bytes"], 8);
    assert_eq!(output["before"]["executable"], output["executable"]);
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
        workspace_paths: vec!["nested/file".into(), "empty".into(), "script".into()],
        write: true,
        child_unguarded: crate::tools::UnguardedGrant::Denied,
        child_web: false,
        commands: vec![pinned, copied_program],
        skills: Vec::new(),
        mcp: Vec::new(),
        runtime_grants: Vec::new(),
        git: None,
        git_identity: None,
        locale: Default::default(),
        environment_sha256: None,
        network: None,
        web_search: None,
    };
    for case in ["fresh", "changed-input", "changed-executable"] {
        let target = temp.path().join(case);
        std::fs::create_dir_all(target.join("project")).unwrap();
        std::fs::create_dir(target.join("runtime")).unwrap();
        let project = super::fs::open_directory(&target.join("project")).unwrap();
        let resources = super::fs::open_directory(&target.join("runtime")).unwrap();
        std::fs::write(&input, b"pinned input").unwrap();
        if case == "changed-input" {
            std::fs::write(&input, b"changed after admission").unwrap();
        }
        if case == "changed-executable" {
            std::fs::write(&copied, b"replaced executable").unwrap();
        }
        let result = super::fs::snapshot_into(&root, &snapshot_config, Some(&project), &resources);
        if case != "fresh" {
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
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest: [1; 32],
                changeset,
            },
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
            "workspace_paths":["."],"write":true,
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
                                format!("y-{index:02}-{}", "\"".repeat(400))
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
            "too-deep" => {
                changeset.files[1].path = vec!["nested"; COMMAND_SNAPSHOT_DEPTH + 1].join("/");
                changeset.files[1].before = None;
                assert!(
                    !changeset.valid(),
                    "the changeset depth limit rejects the batch"
                );
            }
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
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest,
                    changeset,
                },
            },
            &config,
        );
        if case == "too-deep" {
            assert!(
                matches!(result, Err(super::ToolError::Path)),
                "changeset depth rejection"
            );
        }
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
        "workspace_paths":["."],"write":true,
        "commands":[],"skills":[],"mcp":[]
    }))
    .unwrap();
    let result = super::fs::native(
        &workspace,
        &ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
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
                for index in 0..=COMMAND_SNAPSHOT_BYTES / MAX_FILE_BYTES {
                    std::fs::write(
                        command.join(format!("file-{index:02}")),
                        vec![0; MAX_FILE_BYTES],
                    )
                    .unwrap();
                }
            }
            "snapshot-count-overflow" | "changeset-count-limit" | "changeset-count-overflow" => {
                let count = match case {
                    "snapshot-count-overflow" => COMMAND_SNAPSHOT_FILES + 1,
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
                let path = vec![
                    "d";
                    if case == "depth-limit" {
                        COMMAND_SNAPSHOT_DEPTH - 1
                    } else {
                        COMMAND_SNAPSHOT_DEPTH
                    }
                ]
                .join("/");
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
            "excluded" | "set-id-file" | "set-id-directory" => assert!(
                matches!(result, Err(super::ToolError::FilesystemDenied)),
                "explicit filesystem boundary: {case}"
            ),
            _ => assert!(
                matches!(result, Err(super::ToolError::Path)),
                "snapshot validation: {case}"
            ),
        }
    }
}

#[test]
fn delete_and_move_preserve_bytes_and_refuse_stale_or_existing_targets() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let config: Config = serde_json::from_value(
        json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    )
    .unwrap();
    let content = b"binary\0\xff\r\n";
    let source = temp.path().join("source");
    let destination = temp.path().join("nested/destination");
    std::fs::write(&source, content).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    let move_call = ToolCall::Move {
        source: "source".into(),
        destination: "nested/destination".into(),
        expected_digest: Some(hex_digest(content)),
    };
    let stale = ToolCall::Delete {
        path: "source".into(),
        expected_digest: Some(hex_digest(b"stale")),
    };
    assert!(matches!(
        super::fs::native(temp.path(), &stale, &config),
        Err(super::ToolError::Conflict)
    ));
    let output = super::fs::native(temp.path(), &move_call, &config).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"removed":{"sha256":hex_digest(content),"bytes":content.len(),"executable":true}})
    );
    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    assert_ne!(
        std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    std::fs::write(&source, b"replacement").unwrap();
    let occupied = ToolCall::Move {
        source: "source".into(),
        destination: "nested/destination".into(),
        expected_digest: Some(hex_digest(b"replacement")),
    };
    assert!(matches!(
        super::fs::native(temp.path(), &occupied, &config),
        Err(super::ToolError::Conflict)
    ));
    assert_eq!(std::fs::read(&source).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    let delete = ToolCall::Delete {
        path: "nested/destination".into(),
        expected_digest: Some(hex_digest(content)),
    };
    assert_eq!(
        super::fs::native(temp.path(), &delete, &config).unwrap(),
        output
    );
    assert!(!destination.exists());
    assert!(matches!(
        super::fs::native(temp.path(), &delete, &config),
        Err(super::ToolError::NotFound)
    ));
}

#[test]
fn delete_and_move_enforce_path_grants_links_and_file_bounds() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let config: Config = serde_json::from_value(
        json!({"workspace_paths":["owned"],"write":true,"commands":[],"skills":[],"mcp":[]}),
    )
    .unwrap();
    std::fs::create_dir(temp.path().join("owned")).unwrap();
    std::fs::write(temp.path().join("owned/source"), b"source").unwrap();
    symlink("source", temp.path().join("owned/link")).unwrap();
    std::fs::hard_link(
        temp.path().join("owned/source"),
        temp.path().join("owned/hardlink"),
    )
    .unwrap();
    for path in ["owned/link", "owned/hardlink", "owned"] {
        for call in [
            ToolCall::Delete {
                path: path.into(),
                expected_digest: Some(hex_digest(b"source")),
            },
            ToolCall::Move {
                source: path.into(),
                destination: "owned/new".into(),
                expected_digest: Some(hex_digest(b"source")),
            },
        ] {
            assert!(
                super::fs::native(temp.path(), &call, &config).is_err(),
                "{call:?}"
            );
            assert!(!temp.path().join("owned/new").exists());
        }
    }
    std::fs::remove_file(temp.path().join("owned/hardlink")).unwrap();
    symlink("..", temp.path().join("owned/escape")).unwrap();
    for destination in ["owned/link", "owned/escape/outside"] {
        let call = ToolCall::Move {
            source: "owned/source".into(),
            destination: destination.into(),
            expected_digest: Some(hex_digest(b"source")),
        };
        assert!(super::fs::native(temp.path(), &call, &config).is_err());
    }
    assert!(!temp.path().join("outside").exists());
    for (source, destination) in [
        ("owned/source", "other/new"),
        ("other/source", "owned/new"),
        ("owned/source", "owned/.ssh/key"),
        ("owned/source", "../new"),
        ("owned/source", "owned/source"),
        ("owned/source", "owned/SOURCE"),
    ] {
        let call = ToolCall::Move {
            source: source.into(),
            destination: destination.into(),
            expected_digest: Some(hex_digest(b"source")),
        };
        assert!(!config.allows(&call), "{call:?}");
    }
    for path in ["../source", "/source", "owned/.env", "owned/.git/config"] {
        assert!(
            !ToolCall::Delete {
                path: path.into(),
                expected_digest: Some(hex_digest(b"source"))
            }
            .valid()
        );
    }
    let file = std::fs::File::create(temp.path().join("owned/large")).unwrap();
    file.set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
    for call in [
        ToolCall::Delete {
            path: "owned/large".into(),
            expected_digest: Some(hex_digest(b"")),
        },
        ToolCall::Move {
            source: "owned/large".into(),
            destination: "owned/new".into(),
            expected_digest: Some(hex_digest(b"")),
        },
    ] {
        assert!(matches!(
            super::fs::native(temp.path(), &call, &config),
            Err(super::ToolError::Limit)
        ));
    }
    assert!(temp.path().join("owned/source").exists());
    assert!(!temp.path().join("owned/new").exists());
}

#[tokio::test]
async fn workspace_mutations_publish_in_process_with_intent_bound_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    );
    let call = ToolCall::Mkdir {
        path: "directory".into(),
    };
    let observed = run_in_process(&runtime, call.clone()).await;
    assert_eq!(observed.disposition, ToolDisposition::Succeeded);
    assert!(observed.valid());
    assert!(
        matches!(&observed.attestation, Some(Attestation::InProcess { receipt: Some(receipt), .. }) if receipt.intent_digest == observed.intent.digest())
    );
    assert!(temp.path().join("directory").is_dir());
    assert_eq!(
        runtime.reservation_overhead(&call),
        std::time::Duration::ZERO
    );
    let call = ToolCall::Mkdir {
        path: "cancelled".into(),
    };
    let intent = runtime.intent(crate::RunId::new(), crate::AgentRunId::new(), call.clone());
    let cancelled = super::Cancellation::new();
    cancelled.cancel();
    assert_eq!(
        runtime.execute(intent, cancelled).await.disposition,
        ToolDisposition::Cancelled
    );
    assert!(!temp.path().join("cancelled").exists());
    let intent = runtime.intent(crate::RunId::new(), crate::AgentRunId::new(), call);
    crate::FaultPlan::fail_file_parent_sync(intent.id);
    let observed = runtime.execute(intent, super::Cancellation::new()).await;
    assert_eq!(observed.disposition, ToolDisposition::Uncertain);
    assert!(observed.valid() && observed.stops_run());
    assert!(temp.path().join("cancelled").is_dir());
    let mut denied = observed;
    denied.disposition = ToolDisposition::Denied;
    denied.enforcement_cause = Some(EnforcementCause::FilesystemDenied);
    assert_eq!(
        denied.enforcement_diagnostic(),
        Some("File enforcement: protected path or set-ID entry refused")
    );
    assert_eq!(
        super::types::model_observation(&denied)["enforcement_cause"],
        denied.enforcement_diagnostic().unwrap()
    );
}

#[tokio::test]
async fn workspace_file_versions_publish_in_process_with_intent_bound_receipts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("src")).unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}),
    );
    let path = "src/file";
    let source_digest = [1; 32];
    for call in [
        json!({"operation":"write","path":path,"expected_digest":null,"content":"before"}),
        json!({"operation":"edit","path":path,"expected_digest":hex_digest(b"before"),"old":"before","new":"middle"}),
        json!({"operation":"multiedit","file_path":path,"expected_digest":hex_digest(b"middle"),"edits":[{"old_string":"middle","new_string":"after","replace_all":false}]}),
        json!({"operation":"mkdir","path":"src/nested"}),
        json!({"operation":"move","source":path,"destination":"src/nested/moved","expected_digest":hex_digest(b"after")}),
        json!({"operation":"delete","path":"src/nested/moved","expected_digest":hex_digest(b"after")}),
        json!({"operation":"apply_changeset","source":{"kind":"command_receipt","source_intent_digest":source_digest,"changeset":{"files":[{"path":path,"before":null,"after":FileVersion::from_bytes(b"published",false)}]}}}),
    ] {
        let call: ToolCall = serde_json::from_value(call).unwrap();
        assert_eq!(call.facts().site, Site::InProcess);
        assert_eq!(
            runtime.reservation_overhead(&call),
            std::time::Duration::ZERO
        );
        let observed = run_in_process(&runtime, call).await;
        assert_eq!(
            observed.disposition,
            ToolDisposition::Succeeded,
            "{observed:?}"
        );
        assert!(observed.valid());
        assert!(
            matches!(&observed.attestation, Some(Attestation::InProcess { receipt: Some(receipt), .. }) if receipt.intent_digest == observed.intent.digest())
        );
    }
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), b"published");
}

use super::catalog::{call_schemas, catalog_call_schemas, plan_contract_property};
use super::config::Config;
use super::skills::parse_json;
use super::types::*;
use crate::session::WorkspaceIdentity;
use serde_json::{Value, json};

fn config_value() -> Value {
    json!({"version":1,"workspace_paths":["src","Cargo.toml"],"write":false,"commands":[],"skills":[],"mcp":[]})
}

#[test]
fn goal_skill_scope_is_intersected_with_admitted_name_and_resource() {
    use super::ToolRuntime;
    use super::config::Skill;
    use crate::session::{GoalBudget, GoalPlanContract, GoalSkillBinding, GoalSuccessCriterion};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    let config = Config {
        version: 1,
        workspace_paths: vec![".agents/skills".into()],
        write: false,
        child_unguarded: crate::tools::UnguardedGrant::Denied,
        commands: Vec::new(),
        skills: vec![Skill {
            name: "review".into(),
            description: "Review guidance".into(),
            directory: "/tmp/skills/review".into(),
            files: BTreeMap::from([
                ("SKILL.md".into(), "0".repeat(64)),
                ("references/note.md".into(), "1".repeat(64)),
            ]),
        }],
        mcp: Vec::new(),
        runtime_grants: Vec::new(),
        git: None,
    };
    let runtime = ToolRuntime {
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        config,
        workspace: PathBuf::new(),
        reads: None,
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        guard_executable: PathBuf::new(),
        enforcement_digest: [1; 32],
        queue_progress: None,
        queue_wait: None,
        active_action_gate: None,
        hook_stdin: None,
        checkpoint_gate: None,
        #[cfg(target_os = "macos")]
        system_programs: None,
        process_enforcement_digest: [1; 32],
    };
    let contract_for = |name: &str, resource: Option<&str>| GoalPlanContract {
        mission: "Read the requested Skill guidance".into(),
        paths: Vec::new(),
        commands: Vec::new(),
        resource_class: ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: vec![GoalSkillBinding {
            name: name.into(),
            resource: resource.map(str::to_owned),
        }],
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(ToolCall::Skill {
                name: name.into(),
                resource: resource.map(str::to_owned),
            }),
        },
        budget: GoalBudget::default(),
    };

    assert!(runtime.allows_goal_contract(&contract_for("review", None)));
    assert!(runtime.allows_goal_contract(&contract_for("review", Some("references/note.md"))));
    assert!(!runtime.allows_goal_contract(&contract_for("review", Some("references/other.md"))));
    assert!(!runtime.allows_goal_contract(&contract_for("undeclared", None)));
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
        json!({"sha256":hex_digest(b"created"),"bytes":7,"executable":false,"before":null})
    );
    assert!(super::fs::native(&root_path, &write, &config).is_err());
    let original = super::fs::open_file(&root, "nested/file").unwrap();
    let edit = ToolCall::Edit {
        path: "nested/file".into(),
        expected_digest: hex_digest(b"admitted"),
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
        version: 1,
        workspace_paths: vec!["nested/file".into(), "empty".into(), "script".into()],
        write: true,
        child_unguarded: crate::tools::UnguardedGrant::Denied,
        commands: vec![pinned, copied_program],
        skills: Vec::new(),
        mcp: Vec::new(),
        runtime_grants: Vec::new(),
        git: None,
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
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest,
                    changeset,
                },
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
            let parsed = parse_json(catalog.as_bytes(), MAX_TOOL_CATALOG_BYTES).unwrap();
            assert!(parsed.is_object());
            let calls = catalog_call_schemas(&parsed);
            let skill = calls
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
    assert_eq!(config.child_unguarded, UnguardedGrant::Denied);
    let mut widened = value.clone();
    widened["child_unguarded"] = json!("human_only");
    assert!(
        serde_json::from_value::<Config>(widened)
            .unwrap()
            .validate()
            .is_err()
    );
    for invalid in [json!(true), json!(null), json!("automatic")] {
        let mut row = value.clone();
        row["child_unguarded"] = invalid;
        assert!(serde_json::from_value::<Config>(row).is_err());
    }
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
        (".ENV", false),
        (".Env.LOCAL", false),
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
        ("src/.Env", false),
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
        serde_json::from_str(include_str!("../../tests/fixtures/tool-calls.json")).unwrap();
    assert_eq!(rows.len(), 10);
    let schema = jsonschema::options()
        .offline()
        .build(&json!({"anyOf": call_schemas()}))
        .unwrap();
    for row in &rows {
        assert!(schema.is_valid(row));
        let call: ToolCall = serde_json::from_value(row.clone()).unwrap();
        assert!(call.valid());
        let mut unknown = row.clone();
        unknown["network"] = json!(true);
        assert!(serde_json::from_value::<ToolCall>(unknown).is_err());
    }
    for (index, key) in [(4, "expected_digest"), (7, "resource")] {
        let mut missing = rows[index].clone();
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
            limit: 65537,
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
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Mkdir {
            path: "src/new".into(),
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    assert!(intent.valid());
    let mut limits = serde_json::to_value(&intent.limits).unwrap();
    let mut partial = limits.clone();
    partial.as_object_mut().unwrap().remove("cache_transfer_ms");
    assert!(serde_json::from_value::<ToolLimits>(partial).is_err());
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
        enforcement_cause: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "observed".into(),
        attestation: Some(Attestation::Guard(guard)),
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    };
    assert!(observation.valid());
    let mut profile_mismatch = observation.clone();
    let Some(Attestation::Guard(receipt)) = &mut profile_mismatch.attestation else {
        unreachable!()
    };
    receipt.limits.resource_profile =
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
    missing.attestation = None;
    assert!(!missing.valid());
    let mut mismatched = observation.clone();
    let Some(Attestation::Guard(receipt)) = &mut mismatched.attestation else {
        unreachable!()
    };
    receipt.limits.max_processes += 1;
    assert!(!mismatched.valid());
    let mut command_intent = observation.intent.clone();
    command_intent.call = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["build".into()],
        cwd: ".".into(),
    };
    let command_guard = GuardReceipt {
        contract_version: 1,
        intent_digest: command_intent.digest(),
        enforcement_digest: command_intent.enforcement_digest,
        limits: command_intent.limits.clone(),
    };
    let uncertain_cache = ToolObservation {
        enforcement_cause: None,
        intent: command_intent,
        disposition: ToolDisposition::Uncertain,
        output: r#"{"exit_code":0,"stdout":"build complete","stderr":"","changeset":{"files":[]}}"#
            .into(),
        attestation: Some(Attestation::Guard(command_guard)),
        cache_update: CacheUpdate::Uncertain,
    };
    assert!(uncertain_cache.valid());
    assert!(uncertain_cache.stops_run());
    let mut worker_observation = uncertain_cache.clone();
    worker_observation.disposition = ToolDisposition::Succeeded;
    worker_observation.cache_update = CacheUpdate::NotAttempted;
    assert!(worker_observation.valid());
    assert!(!worker_observation.stops_run());
    worker_observation.cache_update = CacheUpdate::Failed;
    assert!(worker_observation.valid());
    let mut encoded = serde_json::to_value(&worker_observation).unwrap();
    encoded.as_object_mut().unwrap().remove("cache_update");
    assert!(serde_json::from_value::<ToolObservation>(encoded).is_err());
    let mut succeeded_uncertain_cache = uncertain_cache.clone();
    succeeded_uncertain_cache.disposition = ToolDisposition::Succeeded;
    assert!(succeeded_uncertain_cache.valid());
    assert!(succeeded_uncertain_cache.stops_run());
    assert_eq!(
        succeeded_uncertain_cache.disposition,
        ToolDisposition::Succeeded
    );
    let mut unattempted_error = uncertain_cache.clone();
    unattempted_error.disposition = ToolDisposition::Uncertain;
    unattempted_error.attestation = None;
    unattempted_error.cache_update = CacheUpdate::NotAttempted;
    assert!(unattempted_error.valid());
    assert!(unattempted_error.stops_run());
    let mut widened = intent.clone();
    widened.limits.runtime_ms = MAX_COMMAND_TIMEOUT_MS + 1;
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
        super::ToolError::FilesystemDenied,
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
        ("process_stop", "process cleanup"),
        ("launcher_reap", "launcher reap"),
        ("stderr_drain", "output EOF"),
        ("storage_cleanup", "storage cleanup"),
    ] {
        let error = super::ToolError::GuardBootstrap(
            serde_json::from_value(serde_json::json!({"stage": stage})).unwrap(),
        );
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let receipt =
            super::uncertain_observation(reserved, error, crate::tools::CacheUpdate::NotApplicable);
        assert_eq!(
            receipt.output,
            format!(
                "Guard {label} unconfirmed; effects may have occurred. Do not retry automatically."
            )
        );
        assert!(receipt.valid());
    }
    for (stage, label) in [
        (
            super::guard::GuardStage::ConnectionDeadline,
            "connection deadline",
        ),
        (
            super::guard::GuardStage::PreparationDeadline,
            "preparation deadline",
        ),
    ] {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let failed =
            super::unstarted_observation(reserved.clone(), super::ToolError::GuardRejected(stage));
        assert_eq!(failed.disposition, ToolDisposition::Failed);
        assert!(failed.attestation.is_none());
        assert!(failed.valid() && failed.stops_run());
        assert_eq!(
            failed.output,
            format!("Guard {label} unavailable before dispatch; no operation was dispatched.")
        );
        let uncertain = super::uncertain_observation(
            reserved,
            super::ToolError::GuardRejected(stage),
            crate::tools::CacheUpdate::NotApplicable,
        );
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.attestation.is_none());
        assert!(uncertain.valid() && uncertain.stops_run());
        assert_eq!(
            uncertain.output,
            format!(
                "Guard {label} unconfirmed; effects may have occurred. Do not retry automatically."
            )
        );
    }
    for error in errors {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let uncertain =
            super::uncertain_observation(reserved, error, crate::tools::CacheUpdate::NotApplicable);
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.attestation.is_none());
        assert!(uncertain.valid(), "minimum-budget uncertainty receipt");
        assert!(uncertain.output.ends_with("Do not retry automatically."));
    }
    let mut oversized = observation;
    oversized.output = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
    assert!(!oversized.valid());
}

#[test]
fn guard_error_after_dispatch_marks_only_a_selected_command_cache_not_attempted() {
    let command = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["build".into()],
        cwd: ".".into(),
    };
    assert_eq!(
        super::guard_error_cache_update(&command, true),
        CacheUpdate::NotAttempted
    );
    assert_eq!(
        super::guard_error_cache_update(&command, false),
        CacheUpdate::NotApplicable
    );
    assert_eq!(
        super::guard_error_cache_update(&ToolCall::List { path: "src".into() }, true),
        CacheUpdate::NotApplicable
    );

    let intent = EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: command,
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let observation = super::uncertain_observation(
        intent,
        super::ToolError::Operation,
        CacheUpdate::NotAttempted,
    );
    assert!(observation.valid());
    assert!(observation.stops_run());
    assert!(observation.attestation.is_none());
    assert_eq!(observation.cache_update, CacheUpdate::NotAttempted);
}

#[test]
fn guard_failures_record_their_stage_and_check_per_intent_for_tests_in_every_build() {
    let intent = |call| EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let command = ToolCall::Command {
        program: "sh".into(),
        args: vec![],
        cwd: String::new(),
    };
    let cleanup = intent(command.clone());
    let failure = serde_json::from_str(r#"{"stage":"storage_cleanup","check":"detach_failed"}"#)
        .expect("closed storage cleanup failure");
    let observation = super::uncertain_observation(
        cleanup.clone(),
        super::ToolError::GuardBootstrap(failure),
        CacheUpdate::NotApplicable,
    );
    assert_eq!(
        observation.output,
        "Guard storage cleanup unconfirmed; effects may have occurred. Do not retry automatically."
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(cleanup.id).as_deref(),
        Some("Guard stage=storage cleanup check=detach failed")
    );
    let unstarted = intent(command);
    super::unstarted_observation(
        unstarted.clone(),
        super::ToolError::GuardRejected(super::guard::GuardStage::ConnectionDeadline),
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(unstarted.id).as_deref(),
        Some("Guard stage=connection deadline")
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(uuid::Uuid::now_v7()),
        None
    );
}

#[test]
fn goal_success_criterion_offers_only_admitted_calls() {
    let calls = catalog_call_schemas(&json!({
        "write": false,
        "commands": [{"name": "cargo"}],
        "skills": [],
        "mcp_servers": []
    }));
    let property = plan_contract_property(&calls);
    let success_calls = property["anyOf"][0]["properties"]["success_criterion"]["anyOf"][0]
        ["properties"]["call"]["anyOf"]
        .as_array()
        .expect("admitted success calls");
    let operations: Vec<&str> = success_calls
        .iter()
        .filter_map(|call| call["properties"]["operation"]["enum"][0].as_str())
        .collect();
    assert_eq!(operations, ["command", "command_unguarded"]);
    assert!(
        success_calls
            .iter()
            .all(|call| call["properties"]["program"]["enum"] == json!(["cargo"]))
    );
}

#[test]
fn goal_success_criterion_without_admitted_calls_omits_the_call_alternative() {
    fn assert_no_empty_any_of(value: &Value) {
        match value {
            Value::Object(map) => {
                assert!(
                    map.get("anyOf").is_none_or(|any_of| any_of
                        .as_array()
                        .is_some_and(|alternatives| !alternatives.is_empty())),
                    "JSON Schema anyOf must list at least one alternative"
                );
                map.values().for_each(assert_no_empty_any_of);
            }
            Value::Array(items) => items.iter().for_each(assert_no_empty_any_of),
            _ => {}
        }
    }

    let calls = catalog_call_schemas(&json!({
        "write": false,
        "commands": [],
        "skills": [],
        "mcp_servers": []
    }));
    let property = plan_contract_property(&calls);
    assert_no_empty_any_of(&property);
    let kinds: Vec<&str> = property["anyOf"][0]["properties"]["success_criterion"]["anyOf"]
        .as_array()
        .expect("success criterion alternatives")
        .iter()
        .filter_map(|alternative| alternative["properties"]["kind"]["enum"][0].as_str())
        .collect();
    assert_eq!(kinds, ["workspace_file_digest", "workspace_file_content"]);
}

#[tokio::test]
async fn cache_receipt_states_preserve_success_without_publication_uncertainty() {
    fn make_observation(call: ToolCall, cache_requested_bytes: u64) -> ToolObservation {
        let mut intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: u64::MAX,
            use_count: 1,
        };
        intent.limits.cache_requested_bytes = cache_requested_bytes;
        intent.limits.cache_bytes = cache_requested_bytes;
        intent.limits.cache_candidate_bytes = 0;
        ToolObservation {
            enforcement_cause: None,
            attestation: Some(Attestation::for_site(&intent)),
            intent,
            disposition: ToolDisposition::Succeeded,
            output: "{}".into(),
            cache_update: CacheUpdate::NotAttempted,
        }
    }

    let read = ToolCall::Read {
        path: "src/main.rs".into(),
        offset: 0,
        limit: 1024,
    };
    let mut observation = make_observation(read, 0);
    super::guard::publish_cache_candidate(
        &mut observation,
        super::guard::CacheCandidate::Received,
        None,
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert_eq!(observation.cache_update, CacheUpdate::NotApplicable);
    assert!(observation.valid());
    assert!(!observation.stops_run());

    for (candidate, requested, expected_update, expected_disposition) in [
        (
            super::guard::CacheCandidate::Received,
            0,
            CacheUpdate::NotApplicable,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Received,
            1,
            CacheUpdate::Rejected,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Rejected,
            1,
            CacheUpdate::Rejected,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Failed,
            1,
            CacheUpdate::Failed,
            ToolDisposition::Succeeded,
        ),
        (
            super::guard::CacheCandidate::Uncertain,
            1,
            CacheUpdate::Uncertain,
            ToolDisposition::Succeeded,
        ),
    ] {
        let call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["check".into()],
            cwd: ".".into(),
        };
        let mut observation = make_observation(call, requested);
        super::guard::publish_cache_candidate(
            &mut observation,
            candidate,
            None,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await;
        assert_eq!(observation.cache_update, expected_update);
        assert_eq!(observation.disposition, expected_disposition);
        assert!(observation.valid());
        assert_eq!(
            observation.stops_run(),
            expected_update == CacheUpdate::Uncertain
        );
    }
}

#[test]
fn revision_grammar_admits_names_and_ancestry_only() {
    let long = "a".repeat(128);
    for value in [
        "HEAD",
        "main",
        "feature/x-1",
        "v1.2.3",
        "HEAD~2",
        "abc1234^",
        "origin/main~1^2",
        "main~",
        long.as_str(),
    ] {
        assert!(valid_revision(value), "{value}");
    }
    let too_long = "a".repeat(129);
    for value in [
        "",
        "-p",
        "--output=x",
        "HEAD:src",
        "@{-1}",
        "main..dev",
        "a b",
        "refs/.hidden",
        "x.lock",
        "a/../b",
        "HEAD^{tree}",
        "~1",
        "main~12345",
        "main~1~1~1~1~1",
        too_long.as_str(),
        "feature/",
        "a//b",
    ] {
        assert!(!valid_revision(value), "{value}");
    }
}

#[test]
fn git_read_calls_validate_paths_combinations_and_counts() {
    let valid = [
        ToolCall::GitStatus { path: ".".into() },
        ToolCall::GitStatus {
            path: String::new(),
        },
        ToolCall::GitDiff {
            path: "src".into(),
            base: None,
            head: None,
            staged: false,
        },
        ToolCall::GitDiff {
            path: "src".into(),
            base: None,
            head: None,
            staged: true,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("HEAD~1".into()),
            head: Some("HEAD".into()),
            staged: false,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("main".into()),
            head: None,
            staged: true,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 1,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("main".into()),
            max_count: 50,
        },
    ];
    for call in &valid {
        assert!(call.valid(), "{call:?}");
        assert!(call.is_goal_plan_prelude() && call.is_git_read() && !call.mutates());
        assert!(!call.is_process() && !call.is_child_research());
    }
    assert_eq!(valid[0].label(), "git_status");
    assert_eq!(valid[2].label(), "git_diff");
    assert_eq!(valid[6].label(), "git_log");
    let invalid = [
        ToolCall::GitStatus {
            path: ".git".into(),
        },
        ToolCall::GitStatus {
            path: ".env".into(),
        },
        ToolCall::GitStatus {
            path: "../x".into(),
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: Some("HEAD".into()),
            staged: false,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("a".into()),
            head: Some("b".into()),
            staged: true,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("-p".into()),
            head: None,
            staged: false,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 0,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 51,
        },
    ];
    for call in &invalid {
        assert!(!call.valid(), "{call:?}");
    }
    let missing_head = json!({"operation":"git_diff","path":".","base":null,"staged":false});
    assert!(serde_json::from_value::<ToolCall>(missing_head).is_err());
    let with_head =
        json!({"operation":"git_diff","path":".","base":null,"head":null,"staged":false});
    assert_eq!(
        serde_json::from_value::<ToolCall>(with_head).unwrap(),
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: None,
            staged: false
        }
    );
}

#[test]
fn git_pin_is_optional_closed_and_rooted_per_platform() {
    let good = if cfg!(target_os = "macos") {
        "/Library/Developer/CommandLineTools/usr/bin/git"
    } else {
        "/usr/bin/git"
    };
    let other = if cfg!(target_os = "macos") {
        "/usr/bin/git"
    } else {
        "/Library/Developer/CommandLineTools/usr/bin/git"
    };
    let pin = json!({"name":"git","executable":good,"sha256":"0".repeat(64),"interpreter":false,"inputs":[]});
    let config_with = |workspace: Value, git: Option<Value>| {
        let mut value = config_value();
        value["workspace_paths"] = workspace;
        if let Some(git) = git {
            value["git"] = git;
        }
        serde_json::from_value::<Config>(value).unwrap()
    };
    let status = ToolCall::GitStatus { path: ".".into() };
    let config = config_with(json!(["."]), Some(pin.clone()));
    assert!(config.validate().is_ok());
    assert!(config.allows(&status));
    assert!(config.allows(&ToolCall::GitLog {
        path: ".".into(),
        revision: None,
        max_count: 5
    }));
    assert!(config.allows(&ToolCall::GitDiff {
        path: "src".into(),
        base: None,
        head: None,
        staged: false,
    }));
    let mutations = [
        json!({"name":"git","executable":other,"sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"sh","executable":if cfg!(target_os = "macos") { "/Library/Developer/CommandLineTools/usr/bin/git" } else { "/usr/bin/git" },"sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}),
        json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[{"path":"/usr/bin/true","destination":"t","sha256":"0".repeat(64)}]}),
        json!({"name":"git","executable":"/opt/git/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"git","executable":"/usr/../opt/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
    ];
    for mutation in mutations {
        assert!(
            config_with(json!(["."]), Some(mutation))
                .validate()
                .is_err()
        );
    }
    let narrow = config_with(json!(["src", "Cargo.toml"]), Some(pin));
    assert!(narrow.validate().is_ok());
    assert!(!narrow.allows(&status));
    let absent = config_with(json!(["."]), None);
    assert!(absent.validate().is_ok());
    assert!(!absent.allows(&status));
    assert!(serde_json::to_value(&absent).unwrap().get("git").is_none());
}

#[test]
fn unguarded_command_is_offered_only_beside_a_cataloged_command_and_never_to_researchers() {
    let names = |catalog: Value| -> Vec<String> {
        catalog_call_schemas(&catalog)
            .iter()
            .filter_map(|call| call["properties"]["operation"]["enum"][0].as_str())
            .map(str::to_owned)
            .collect()
    };
    let without = json!({"write": false, "commands": [], "skills": [], "mcp_servers": []});
    assert!(!names(without).contains(&"command_unguarded".to_owned()));
    let with =
        json!({"write": false, "commands": [{"name": "sh"}], "skills": [], "mcp_servers": []});
    let calls = catalog_call_schemas(&with);
    let unguarded = calls
        .iter()
        .find(|call| call["properties"]["operation"]["enum"][0] == "command_unguarded")
        .expect("offered beside a cataloged command");
    assert_eq!(unguarded["properties"]["program"]["enum"], json!(["sh"]));
    assert!(
        unguarded["required"]
            .as_array()
            .unwrap()
            .contains(&json!("justification"))
    );
    assert!(
        !super::ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: Vec::new(),
            cwd: String::new(),
            justification: "run".into(),
        }
        .is_child_research()
    );
}

#[test]
fn git_operations_are_offered_only_when_cataloged() {
    let git_operations = ["git_status", "git_diff", "git_log"];
    let offered = |catalog: Value| -> Vec<String> {
        catalog_call_schemas(&catalog)
            .iter()
            .filter_map(|call| call["properties"]["operation"]["enum"][0].as_str())
            .map(str::to_owned)
            .collect()
    };
    let base = json!({"write": false, "commands": [], "skills": [], "mcp_servers": []});
    let mut cataloged = base.clone();
    cataloged["git"] = json!({"operations": git_operations});
    assert_eq!(
        offered(cataloged),
        [
            "list",
            "read",
            "search",
            "git_status",
            "git_diff",
            "git_log"
        ]
    );
    for git in [Some(json!("unavailable")), None] {
        let mut value = base.clone();
        if let Some(git) = git {
            value["git"] = git;
        }
        assert!(
            offered(value)
                .iter()
                .all(|name| !git_operations.contains(&name.as_str()))
        );
    }
}

#[test]
fn git_catalog_is_unavailable_without_a_pin_and_names_operations_with_one() {
    use super::ToolRuntime;
    use std::path::PathBuf;

    let runtime = |config: Config| ToolRuntime {
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        config,
        workspace: PathBuf::new(),
        reads: None,
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        guard_executable: PathBuf::new(),
        enforcement_digest: [1; 32],
        queue_progress: None,
        queue_wait: None,
        active_action_gate: None,
        hook_stdin: None,
        checkpoint_gate: None,
        #[cfg(target_os = "macos")]
        system_programs: None,
        process_enforcement_digest: [1; 32],
    };
    let mut value = config_value();
    value["workspace_paths"] = json!(["."]);
    let mut config: Config = serde_json::from_value(value).unwrap();
    let catalog = |config: Config| {
        let context = runtime(config).context().unwrap();
        parse_json(context.catalog.as_bytes(), MAX_TOOL_CATALOG_BYTES).unwrap()
    };
    assert_eq!(catalog(config.clone())["git"], "unavailable");
    config.git = Some(
        serde_json::from_value(json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]})).unwrap(),
    );
    assert_eq!(
        catalog(config)["git"]["operations"],
        json!(["git_status", "git_diff", "git_log"])
    );
}

#[test]
fn child_research_excludes_git_reads() {
    for call in [
        ToolCall::GitStatus { path: ".".into() },
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: None,
            staged: false,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 1,
        },
    ] {
        assert!(call.is_goal_plan_prelude());
        assert!(!call.is_child_research(), "{call:?}");
    }
    for call in [
        ToolCall::List { path: ".".into() },
        ToolCall::Read {
            path: "a".into(),
            offset: 0,
            limit: 1,
        },
        ToolCall::Search {
            path: ".".into(),
            query: "x".into(),
        },
        ToolCall::Skill {
            name: "review".into(),
            resource: None,
        },
        ToolCall::McpList {
            server: "local".into(),
        },
    ] {
        assert!(call.is_child_research(), "{call:?}");
    }
    for call in [
        ToolCall::McpCall {
            server: "local".into(),
            tool: "probe".into(),
            schema_digest: "0".repeat(64),
            arguments: "{}".into(),
        },
        ToolCall::Write {
            path: "a".into(),
            expected_digest: None,
            content: "x".into(),
        },
    ] {
        assert!(!call.is_child_research(), "{call:?}");
    }
}

#[test]
fn model_catalog_prose_matches_the_checked_in_snapshot() {
    use super::ToolRuntime;
    use std::path::PathBuf;

    let program = json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]});
    let value = json!({
        "version":1,"workspace_paths":["src","Cargo.toml"],"write":true,
        "commands":[{"name":"bash","executable":"/bin/bash","sha256":"1".repeat(64),"interpreter":true,"inputs":[]}],
        "skills":[{"name":"review","description":"Review guidance","directory":"/tmp/skills/review","files":{"SKILL.md":"2".repeat(64)}}],
        "mcp":[{"name":"local","program":program,"args":[],"tools":["probe"]}],
        "git": program,
    });
    let runtime = ToolRuntime {
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        config: serde_json::from_value(value).unwrap(),
        workspace: PathBuf::new(),
        reads: None,
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        guard_executable: PathBuf::new(),
        enforcement_digest: [1; 32],
        queue_progress: None,
        queue_wait: None,
        active_action_gate: None,
        hook_stdin: None,
        checkpoint_gate: None,
        #[cfg(target_os = "macos")]
        system_programs: None,
        process_enforcement_digest: [1; 32],
    };
    let render = |catalog: String| {
        let mut value: Value = serde_json::from_str(&catalog).unwrap();
        value["host_clock"]["unix_ms"] = json!(0);
        value
    };
    let rendered = json!({
        "primary": render(runtime.context().unwrap().catalog),
        "researcher": render(runtime.research_context(3).unwrap().catalog),
    });
    let rendered = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
    // Regenerate with ARANY_UPDATE_SNAPSHOTS=1 (per OS), then review the diff.
    if std::env::var_os("ARANY_UPDATE_SNAPSHOTS").is_some() {
        let file = if cfg!(target_os = "macos") {
            "catalog_prose_macos"
        } else {
            "catalog_prose"
        };
        std::fs::write(
            format!(
                "{}/src/tools/snapshots/{file}.json",
                env!("CARGO_MANIFEST_DIR")
            ),
            &rendered,
        )
        .unwrap();
    }
    let expected = if cfg!(target_os = "macos") {
        include_str!("snapshots/catalog_prose_macos.json")
    } else {
        include_str!("snapshots/catalog_prose.json")
    };
    assert!(rendered == expected, "{rendered}");
}

pub(super) fn in_process_runtime(workspace: &std::path::Path, config: Value) -> super::ToolRuntime {
    let root = super::fs::open_directory(workspace).unwrap();
    super::ToolRuntime {
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        config: serde_json::from_value(config).unwrap(),
        workspace: workspace.to_owned(),
        workspace_identity: super::fs::workspace_identity(&root).unwrap(),
        reads: Some(super::InProcessReads {
            root: std::sync::Arc::new(root),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(super::MAX_IN_PROCESS_READS)),
        }),
        guard_executable: std::path::PathBuf::new(),
        enforcement_digest: [1; 32],
        queue_progress: None,
        queue_wait: None,
        active_action_gate: None,
        hook_stdin: None,
        checkpoint_gate: None,
        #[cfg(target_os = "macos")]
        system_programs: None,
        process_enforcement_digest: [1; 32],
    }
}

#[test]
fn unguarded_rules_snapshot_does_not_see_a_rule_recorded_later() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut runtime = in_process_runtime(&workspace, config_value());
    runtime.cache_state = Some(std::sync::Arc::new(state));
    let call = |cwd: &str| ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec!["-c".into(), "cargo test".into()],
        cwd: cwd.into(),
        justification: "Allow this exact command?".into(),
    };
    assert!(!runtime.matches_exact_command_rule(&call("")));
    assert_eq!(runtime.receipt().unguarded_rules_digest, None);
    runtime.record_exact_command_rule(&call("")).unwrap();
    let state = runtime.cache_state.as_deref().unwrap();
    runtime.unguarded_rules = super::ToolRuntime::load_exact_command_rules(state).unwrap();
    let admitted_digest = runtime.receipt().unguarded_rules_digest.unwrap();
    assert!(runtime.matches_exact_command_rule(&call("")));
    runtime.record_exact_command_rule(&call("src")).unwrap();
    assert!(!runtime.matches_exact_command_rule(&call("src")));
    assert_eq!(
        runtime.receipt().unguarded_rules_digest,
        Some(admitted_digest)
    );
    let mut refreshed = runtime.clone();
    refreshed.unguarded_rules =
        super::ToolRuntime::load_exact_command_rules(runtime.cache_state.as_deref().unwrap())
            .unwrap();
    assert!(refreshed.matches_exact_command_rule(&call("src")));
    assert_ne!(
        refreshed.receipt().unguarded_rules_digest,
        Some(admitted_digest)
    );
    refreshed.workspace_identity.inode = refreshed.workspace_identity.inode.wrapping_add(1);
    assert!(!refreshed.matches_exact_command_rule(&call("")));
    let mut another_path = runtime;
    another_path.workspace = temp.path().join("another-workspace");
    assert!(!another_path.matches_exact_command_rule(&call("")));
}

async fn run_in_process(runtime: &super::ToolRuntime, call: ToolCall) -> ToolObservation {
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call,
    );
    runtime.execute(intent, super::Cancellation::new()).await
}

#[tokio::test]
async fn in_process_reads_attest_and_refuse_secrets_links_and_a_replaced_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(workspace.join("src/.env"), "TOKEN=secret-canary\n").unwrap();
    std::fs::write(temp.path().join("outside.txt"), "outside-canary").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let read = |path: &str| ToolCall::Read {
        path: path.into(),
        offset: 0,
        limit: MAX_READ_PAGE_BYTES,
    };

    let observation = run_in_process(&runtime, read("src/main.rs")).await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(observation.valid());
    assert!(matches!(
        observation.attestation,
        Some(Attestation::InProcess { workspace_device, workspace_inode, .. })
            if WorkspaceIdentity { device: workspace_device, inode: workspace_inode } == runtime.workspace_identity
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&observation.output).unwrap()["text"],
        "fn main() {}\n"
    );
    assert_eq!(
        runtime.reservation_overhead(&read("src/main.rs")),
        std::time::Duration::ZERO
    );
    assert!(
        runtime.reservation_overhead(&ToolCall::Mkdir {
            path: "src/new".into()
        }) > std::time::Duration::ZERO
    );

    let secret = run_in_process(&runtime, read("src/.env")).await;
    assert_eq!(secret.disposition, ToolDisposition::Denied);
    assert!(secret.attestation.is_none());
    for listing in [
        ToolCall::List { path: "src".into() },
        ToolCall::Search {
            path: "src".into(),
            query: "canary".into(),
        },
    ] {
        let observation = run_in_process(&runtime, listing).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(
            !observation.output.contains(".env"),
            "{}",
            observation.output
        );
        assert!(
            !observation.output.contains("canary"),
            "{}",
            observation.output
        );
    }

    std::os::unix::fs::symlink(
        temp.path().join("outside.txt"),
        workspace.join("src/escape"),
    )
    .unwrap();
    let escape = run_in_process(&runtime, read("src/escape")).await;
    assert_eq!(escape.disposition, ToolDisposition::Denied);
    assert!(!escape.output.contains("outside-canary"));
    assert!(escape.valid());

    std::fs::rename(&workspace, temp.path().join("moved")).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "replacement").unwrap();
    let replaced = run_in_process(&runtime, read("src/main.rs")).await;
    assert_eq!(replaced.disposition, ToolDisposition::Failed);
    assert!(replaced.attestation.is_none());
    assert!(replaced.stops_run());
    assert!(
        replaced.output.contains("pinned input"),
        "{}",
        replaced.output
    );
    assert!(!replaced.output.contains("replacement"));

    let mut expired_intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        read("src/main.rs"),
    );
    expired_intent.expires_at_ms = 1;
    let expired = runtime
        .execute(expired_intent, super::Cancellation::new())
        .await;
    assert_eq!(expired.disposition, ToolDisposition::Failed);
    assert!(expired.attestation.is_none());
}

#[tokio::test]
async fn listing_without_a_fitting_row_is_a_limit_and_empty_results_succeed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let long_name = format!("{}.txt", "a".repeat(160));
    std::fs::write(workspace.join("src").join(&long_name), "quoted").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, result_bytes) in [
        (
            ToolCall::Search {
                path: "src".into(),
                query: "quoted".into(),
            },
            128,
        ),
        (ToolCall::List { path: "src".into() }, 128),
    ] {
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = result_bytes;
        let observation = runtime.execute(intent, super::Cancellation::new()).await;
        assert_eq!(
            observation.disposition,
            ToolDisposition::Limit,
            "{call:?} with one unfit row"
        );
    }
    for call in [
        ToolCall::Search {
            path: "src".into(),
            query: "absent".into(),
        },
        ToolCall::List {
            path: "empty".into(),
        },
    ] {
        std::fs::create_dir_all(workspace.join("empty")).unwrap();
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = 1024;
        let observation = runtime.execute(intent, super::Cancellation::new()).await;
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{call:?} with no matches"
        );
        let rows: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(rows["entries"], json!([]));
    }
}

#[test]
fn result_capacity_is_the_context_fit_formula_with_a_floor_and_the_general_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = in_process_runtime(
        temp.path(),
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.context_limit_bytes = 1 << 20;
    let limit = runtime.context_limit_bytes();
    let fitted = |overhead: usize| limit.saturating_sub(overhead + 512) / 6;
    let ceiling = |overhead: usize| {
        runtime
            .result_capacity(overhead, 0)
            .map(|capacity| capacity.min(MAX_TOOL_RESULT_BYTES))
    };
    let below_ceiling = limit - 512 - 6 * (MAX_TOOL_RESULT_BYTES / 2);
    assert_eq!(
        runtime.result_capacity(below_ceiling, 0),
        Some(fitted(below_ceiling))
    );
    assert_eq!(ceiling(below_ceiling), Some(MAX_TOOL_RESULT_BYTES / 2));
    assert!(
        fitted(0) > MAX_TOOL_RESULT_BYTES,
        "the context fit exceeds the ceiling at zero overhead"
    );
    assert_eq!(ceiling(0), Some(MAX_TOOL_RESULT_BYTES));
    let at_floor = limit - 512 - 6 * 128;
    assert_eq!(runtime.result_capacity(at_floor, 0), Some(128));
    assert_eq!(runtime.result_capacity(at_floor + 1, 0), None);
}

#[tokio::test]
async fn in_process_results_fit_their_budget_before_serialization() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let text = "\"quoted\\line\"\n".repeat(16 * 1024);
    std::fs::write(workspace.join("src/big.txt"), &text).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for result_bytes in [256, 4096, MAX_TOOL_RESULT_BYTES as u32] {
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            ToolCall::Read {
                path: "src/big.txt".into(),
                offset: 0,
                limit: MAX_READ_PAGE_BYTES,
            },
        );
        intent.limits.result_bytes = result_bytes;
        let observation = runtime.execute(intent, super::Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.output.len() <= result_bytes as usize);
        let page: Value = serde_json::from_str(&observation.output).unwrap();
        let next = page["next_offset"].as_u64().unwrap() as usize;
        assert!(next > 0 && next < text.len());
        assert_eq!(page["truncated"], true);
        assert_eq!(page["text"], text[..next]);
    }
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Search {
            path: "src".into(),
            query: "quoted".into(),
        },
    );
    intent.limits.result_bytes = 1024;
    let observation = runtime.execute(intent, super::Cancellation::new()).await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(observation.output.len() <= 1024);
    let rows: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(rows["may_be_truncated"], true);
    assert!(!rows["entries"].as_array().unwrap().is_empty());
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Read {
            path: "src/big.txt".into(),
            offset: 0,
            limit: 1,
        },
    );
    intent.limits.result_bytes = 128;
    let observation = runtime.execute(intent, super::Cancellation::new()).await;
    assert_eq!(observation.disposition, ToolDisposition::Limit);
}

#[test]
fn the_primary_prompt_states_the_read_page_limit() {
    assert!(
        include_str!("../provider/prompts/primary.txt")
            .contains(&format!("a limit from 1 to {MAX_READ_PAGE_BYTES}."))
    );
}

#[tokio::test]
async fn in_process_repeated_reads_all_succeed_on_a_large_file() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n".repeat(4096)).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for _ in 0..40 {
        let observation = run_in_process(
            &runtime,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: MAX_READ_PAGE_BYTES,
            },
        )
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    }
}

#[tokio::test]
async fn in_process_reads_are_refused_before_dispatch_when_every_slot_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let slots = std::sync::Arc::clone(&runtime.reads.as_ref().unwrap().slots);
    let held = slots
        .try_acquire_many_owned(super::MAX_IN_PROCESS_READS as u32)
        .unwrap();
    let refused = run_in_process(
        &runtime,
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: 0,
            limit: 16,
        },
    )
    .await;
    drop(held);
    assert_eq!(refused.disposition, ToolDisposition::Limit);
    assert!(refused.attestation.is_none());
    assert!(!refused.stops_run());
    assert!(!refused.output.contains("fn main"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn hook_task_fit_check_matches_the_encoded_task_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: String::new(),
        },
    );
    assert!(runtime.with_hook_stdin("{}").hook_task_fits(&intent));
    let padded = "x".repeat(super::guard::TASK_BYTES_CAP);
    assert!(!runtime.with_hook_stdin(&padded).hook_task_fits(&intent));
}

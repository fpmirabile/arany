use super::catalog::{Family, Site, call_schemas, catalog_call_schemas, plan_schema};
use super::config::Config;
use super::skills::parse_json;
use super::types::*;
use crate::session::WorkspaceIdentity;
use crate::validate::{MAX_REVISION_BYTES, digest, hex_digest, valid_revision};
use serde_json::{Value, json};

fn config_value() -> Value {
    json!({"version":1,"workspace_paths":["src","Cargo.toml"],"write":false,"commands":[],"skills":[],"mcp":[]})
}

#[test]
fn tool_call_site_and_family_predicates_match_facts_for_every_variant() {
    use crate::session::worker::{ChangesetSource, WorkerDataPurpose, WorkerPreparation};
    use crate::tools::types::CommandChangeset;

    let path = || "src".to_owned();
    let calls = [
        ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: [1; 32],
        },
        ToolCall::List { path: path() },
        ToolCall::Read {
            path: path(),
            offset: None,
            limit: Some(1),
        },
        ToolCall::Search {
            path: path(),
            query: "q".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: MAX_SEARCH_ROWS,
        },
        ToolCall::Delete {
            path: path(),
            expected_digest: "0".repeat(64),
        },
        ToolCall::Move {
            source: path(),
            destination: "destination".into(),
            expected_digest: "0".repeat(64),
        },
        ToolCall::Mkdir { path: path() },
        ToolCall::Write {
            path: path(),
            expected_digest: None,
            content: String::new(),
        },
        ToolCall::Edit {
            path: path(),
            expected_digest: "0".repeat(64),
            old: "a".into(),
            new: "b".into(),
        },
        ToolCall::MultiEdit {
            file_path: path(),
            expected_digest: "0".repeat(64),
            edits: Vec::new(),
        },
        ToolCall::Command {
            program: "cargo".into(),
            args: Vec::new(),
            cwd: ".".into(),
        },
        ToolCall::CommandUnguarded {
            program: "cargo".into(),
            args: Vec::new(),
            cwd: ".".into(),
            justification: "Run the failed command once on the host".into(),
            prefix_rule: None,
        },
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt {
                source_intent_digest: [1; 32],
                changeset: CommandChangeset::default(),
            },
        },
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: crate::session::AgentRunId::new(),
                purpose: WorkerDataPurpose::Base,
                max_chunks: 1,
                max_bytes: 1,
            },
        },
        ToolCall::PrepareWorker {
            preparation: WorkerPreparation {
                assignment_id: crate::session::AgentRunId::new(),
                purpose: WorkerDataPurpose::Handover,
                max_chunks: 1,
                max_bytes: 1,
            },
        },
        ToolCall::Skill {
            name: "guide".into(),
            resource: None,
        },
        ToolCall::McpList {
            server: "local".into(),
        },
        ToolCall::McpCall {
            server: "local".into(),
            tool: "t".into(),
            schema_digest: "0".repeat(64),
            arguments: "{}".into(),
        },
        ToolCall::GitStatus { path: path() },
        ToolCall::GitDiff {
            path: path(),
            base: None,
            head: None,
            staged: false,
        },
        ToolCall::GitLog {
            path: path(),
            revision: None,
            max_count: 1,
        },
        ToolCall::GitCommit {
            message: "m".into(),
            paths: vec![path()],
            staged: false,
        },
        ToolCall::GitBranch {
            name: None,
            start_point: None,
            scope: None,
            base: None,
            max_count: None,
        },
        ToolCall::GitBranch {
            name: Some("topic".into()),
            start_point: None,
            scope: None,
            base: None,
            max_count: None,
        },
        ToolCall::GitSwitch {
            branch: "topic".into(),
        },
        ToolCall::GitPush {
            remote: "https://github.com/o/r.git".into(),
            branch: "topic".into(),
        },
        ToolCall::GhPrCreate {
            repository: "github.com/o/r".into(),
            base: "main".into(),
            head: "topic".into(),
            title: "T".into(),
            body: String::new(),
            draft: true,
        },
        ToolCall::Glob {
            pattern: "**/*.rs".into(),
            path: path(),
            limit: 1,
        },
    ];

    for call in calls {
        let old_is_host = matches!(call, ToolCall::GitPush { .. } | ToolCall::GhPrCreate { .. });
        let old_is_unguarded = matches!(call, ToolCall::CommandUnguarded { .. });
        let old_is_internal = matches!(
            call,
            ToolCall::ParseAgentDefinition { .. } | ToolCall::PrepareWorker { .. }
        );

        assert_eq!(call.is_host(), call.facts().site == Site::Host);
        assert_eq!(call.is_host(), old_is_host, "{call:?}");
        assert_eq!(call.is_unguarded(), call.facts().site == Site::Unguarded);
        assert_eq!(call.is_unguarded(), old_is_unguarded, "{call:?}");
        assert_eq!(
            call.is_internal(),
            call.facts().family == Family::Preparation
        );
        assert_eq!(call.is_internal(), old_is_internal, "{call:?}");
    }
}

#[test]
fn multiedit_orders_and_bounds_the_whole_publication() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    let config: Config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap();
    let edit = |old: &str, new: &str, replace_all| LiteralEdit {
        old_string: old.into(),
        new_string: new.into(),
        replace_all,
    };
    let call = |bytes: &[u8], edits| ToolCall::MultiEdit {
        file_path: "file".into(),
        expected_digest: hex_digest(bytes),
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
            disable_model_invocation: false,
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
        git_identity: None,
    };
    let runtime = ToolRuntime {
        host_settings_admitted: true,
        host_settings_saved: Default::default(),
        guard_available: true,
        git_writes_available: false,
        host_authentication: Default::default(),
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
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
        git_identity: None,
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
fn command_changeset_limits_cover_content_serialization_and_internal_context() {
    use crate::session::worker::ChangesetSource;

    let exact_content = CommandChangeset {
        files: vec![FileChange {
            path: "content".into(),
            before: Some(FileVersion::from_bytes(&vec![b'a'; 16 * 1024], false)),
            after: Some(FileVersion::from_bytes(&vec![b'b'; 16 * 1024], false)),
        }],
    };
    assert!(exact_content.valid(), "32 KiB combined content is admitted");
    let overflow_content = CommandChangeset {
        files: vec![FileChange {
            path: "content".into(),
            before: exact_content.files[0].before.clone(),
            after: Some(FileVersion::from_bytes(&vec![b'b'; 16 * 1024 + 1], false)),
        }],
    };
    assert!(
        !overflow_content.valid(),
        "combined content over 32 KiB rejects"
    );

    let long_escaped_paths = |components: usize| CommandChangeset {
        files: (0..MAX_CHANGESET_FILES)
            .map(|index| FileChange {
                path: format!(
                    "f{index:02}/{}",
                    std::iter::repeat_n("\"".repeat(100), components)
                        .collect::<Vec<_>>()
                        .join("/")
                ),
                before: None,
                after: Some(FileVersion::from_bytes(b"", false)),
            })
            .collect(),
    };
    let admitted = long_escaped_paths(3);
    let admitted_bytes = serde_json::to_vec(&admitted).unwrap();
    assert!(admitted_bytes.len() <= MAX_CHANGESET_SERIALIZED_BYTES);
    assert!(
        admitted.valid(),
        "long escaped paths fit the serialized ceiling"
    );
    assert!(
        !long_escaped_paths(4).valid(),
        "serialized overflow rejects"
    );

    let integration = ToolCall::ApplyChangeset {
        source: ChangesetSource::CommandReceipt {
            source_intent_digest: [1; 32],
            changeset: admitted,
        },
    };
    assert_eq!(
        integration.argument_limit(),
        MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES
    );
    assert!(
        serde_json::to_vec(&integration).unwrap().len() <= integration.argument_limit(),
        "the internal envelope admits the largest serialized command changeset"
    );
    assert!(
        super::ToolRuntime::step_context_reserve(true, 0)
            >= MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES + 128 * 6 + 512 + 512,
        "context reserves escaped internal integration arguments"
    );
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
                offset: None,
                limit: Some(1)
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
                offset: None,
                limit: Some(1)
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
    assert_eq!(rows.len(), 14);
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
    // Search options default when omitted or null and serialize omitted, so stored default intents keep their bytes.
    let decoded = |value: Value| serde_json::from_value::<ToolCall>(value).unwrap();
    let historical = json!({"operation":"search","path":"src","query":"fn main"});
    for equivalent in [
        rows[2].clone(),
        json!({"operation":"search","path":"src","query":"fn main","regex":false,"include":null,"case_sensitive":true,"offset":0,"limit":100}),
    ] {
        assert_eq!(decoded(equivalent), decoded(historical.clone()));
    }
    assert_eq!(
        serde_json::to_value(decoded(rows[2].clone())).unwrap(),
        historical
    );
    let custom = json!({"operation":"search","path":"src","query":"fn \\w+","regex":true,"include":"**/*.rs","case_sensitive":false,"offset":5,"limit":7});
    assert_eq!(
        serde_json::to_value(decoded(custom.clone())).unwrap(),
        custom
    );
    for keys in [vec!["offset"], vec!["limit"], vec!["offset", "limit"]] {
        let mut missing = rows[1].clone();
        for key in keys {
            missing.as_object_mut().unwrap().remove(key);
        }
        assert!(!schema.is_valid(&missing));
        let call = decoded(missing);
        assert!(call.valid());
        assert_eq!(serde_json::to_value(call).unwrap(), rows[1]);
    }
    for (index, key) in [(1, "path"), (4, "expected_digest"), (7, "resource")] {
        let mut missing = rows[index].clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ToolCall>(missing).is_err(),
            "fields without defaults are required"
        );
    }
    for call in [
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(2001),
        },
        ToolCall::Search {
            path: "src".into(),
            query: "x".repeat(513),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "x".repeat(MAX_EDIT_CONTENT_BYTES + 1),
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: format!("{}x", "é".repeat(MAX_EDIT_CONTENT_BYTES / 2)),
        },
        ToolCall::Edit {
            path: "src/new".into(),
            expected_digest: "0".repeat(64),
            old: "x".repeat(MAX_EDIT_CONTENT_BYTES / 2),
            new: "y".repeat(MAX_EDIT_CONTENT_BYTES / 2 + 1),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".into(); 65],
            cwd: "".into(),
        },
        // Within its field limits, but over the serialized bound every non-content operation keeps.
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".repeat(300); 64],
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
    // Bytes, not characters, and six-byte escapes plus a long escaped path still fit.
    let escaped_path = "\"".repeat(512);
    for call in [
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "é".repeat(MAX_EDIT_CONTENT_BYTES / 2),
        },
        ToolCall::Write {
            path: escaped_path.clone(),
            expected_digest: Some("0".repeat(64)),
            content: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES),
        },
        ToolCall::Edit {
            path: escaped_path.clone(),
            expected_digest: "0".repeat(64),
            old: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
            new: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
        },
        ToolCall::MultiEdit {
            file_path: escaped_path,
            expected_digest: "0".repeat(64),
            edits: vec![LiteralEdit {
                old_string: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                new_string: "\u{2}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                replace_all: false,
            }],
        },
    ] {
        assert!(call.valid());
        let bytes = serde_json::to_vec(&call).unwrap().len();
        assert!(bytes > MAX_ORDINARY_TOOL_ARGUMENT_BYTES && bytes <= MAX_TOOL_ARGUMENT_BYTES);
    }
    let intent = EffectIntent {
        host: None,
        host_command: None,
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
        limit_cause: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "observed".into(),
        attestation: Some(Attestation::Guard(guard)),
        approval_cause: None,
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
        limit_cause: None,
        intent: command_intent,
        disposition: ToolDisposition::Uncertain,
        output: r#"{"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"build complete","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":14,"lines":1},"stderr_total":{"bytes":0,"lines":0}}"#
            .into(),
        attestation: Some(Attestation::Guard(command_guard)),
        approval_cause: None,
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
        host: None,
        host_command: None,
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
        host: None,
        host_command: None,
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
        "host_commands": true,
        "commands": [{"name": "cargo"}],
        "skills": [],
        "mcp_servers": []
    }));
    let property = plan_schema(&calls, &Default::default());
    let success_calls =
        property["properties"]["success_criterion"]["anyOf"][0]["properties"]["call"]["anyOf"]
            .as_array()
            .expect("admitted success calls");
    let operations: Vec<&str> = success_calls
        .iter()
        .filter_map(|call| call["properties"]["operation"]["enum"][0].as_str())
        .collect();
    assert_eq!(operations, ["command", "command_unguarded"]);
    assert!(success_calls.iter().all(|call| {
        if call["properties"]["operation"]["enum"][0] == "command" {
            call["properties"]["program"]["enum"] == json!(["cargo"])
        } else {
            call["properties"]["program"] == json!({"type":"string"})
        }
    }));
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
    let property = plan_schema(&calls, &Default::default());
    assert_no_empty_any_of(&property);
    let kinds: Vec<&str> = property["properties"]["success_criterion"]["anyOf"]
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
            host: None,
            host_command: None,
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
            use_count: 1,
        };
        intent.limits.cache_requested_bytes = cache_requested_bytes;
        intent.limits.cache_bytes = cache_requested_bytes;
        intent.limits.cache_candidate_bytes = 0;
        ToolObservation {
            enforcement_cause: None,
            limit_cause: None,
            attestation: Some(Attestation::for_site(&intent)),
            intent,
            disposition: ToolDisposition::Succeeded,
            output: serde_json::to_string(&super::types::CommandResult {
                selection: Some(Default::default()),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                stdout_total: Default::default(),
                stderr_total: Default::default(),
                changeset: Default::default(),
            })
            .unwrap(),
            approval_cause: None,
            cache_update: CacheUpdate::NotAttempted,
        }
    }

    let read = ToolCall::Read {
        path: "src/main.rs".into(),
        offset: None,
        limit: Some(1024),
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
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("release..feature/topic".into()),
            max_count: 50,
        },
    ];
    for call in &valid {
        assert!(call.valid(), "{call:?}");
        assert!(call.is_goal_plan_prelude() && call.is_git() && !call.mutates());
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
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("..HEAD".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("HEAD..".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("HEAD...main".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("main..topic..other".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some(format!(
                "{}..{}",
                "a".repeat(MAX_REVISION_BYTES),
                "b".repeat(MAX_REVISION_BYTES) + "b"
            )),
            max_count: 5,
        },
    ];
    for call in &invalid {
        assert!(!call.valid(), "{call:?}");
    }
    let legacy = r#"{"operation":"git_branch","name":null,"start_point":null}"#;
    let call: ToolCall = serde_json::from_str(legacy).unwrap();
    assert!(call.valid());
    assert_eq!(serde_json::to_string(&call).unwrap(), legacy);
    let model = model_call(&call);
    for field in ["scope", "base", "max_count"] {
        assert_eq!(model.get(field), Some(&Value::Null));
    }
    for scope in [None, Some("local"), Some("remote"), Some("all")] {
        for count in [None, Some(1), Some(256)] {
            let call = ToolCall::GitBranch {
                name: None,
                start_point: None,
                scope: scope.map(str::to_owned),
                base: Some("HEAD~1".into()),
                max_count: count,
            };
            assert!(call.valid());
            assert!(call.is_goal_plan_prelude() && !call.mutates() && !call.is_child_research());
            assert_eq!(
                serde_json::from_value::<ToolCall>(serde_json::to_value(&call).unwrap()).unwrap(),
                call
            );
        }
    }
    for fields in [
        json!({"scope":"live"}),
        json!({"max_count":0}),
        json!({"max_count":257}),
        json!({"base":"main..topic"}),
        json!({"base":"--all"}),
        json!({"start_point":"main"}),
    ] {
        let mut value: Value = serde_json::from_str(legacy).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(!serde_json::from_value::<ToolCall>(value).unwrap().valid());
    }
    for fields in [
        json!({"scope":"local"}),
        json!({"base":"HEAD"}),
        json!({"max_count":1}),
    ] {
        let mut value = json!({"operation":"git_branch","name":"topic","start_point":null});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(!serde_json::from_value::<ToolCall>(value).unwrap().valid());
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
fn host_command_catalog_is_primary_only_and_needs_no_guarded_program() {
    let names = |catalog: Value| -> Vec<String> {
        catalog_call_schemas(&catalog)
            .iter()
            .filter_map(|call| call["properties"]["operation"]["enum"][0].as_str())
            .map(str::to_owned)
            .collect()
    };
    let without = json!({"write": false, "commands": [], "skills": [], "mcp_servers": []});
    assert!(!names(without).contains(&"command_unguarded".to_owned()));
    let with = json!({"write": false, "host_commands": true, "commands": [], "skills": [], "mcp_servers": []});
    let calls = catalog_call_schemas(&with);
    let unguarded = calls
        .iter()
        .find(|call| call["properties"]["operation"]["enum"][0] == "command_unguarded")
        .expect("offered beside a cataloged command");
    assert!(unguarded["properties"]["program"].get("enum").is_none());
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
            prefix_rule: None,
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
            "git_log",
            "glob"
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

    let runtime = |config: Config, git_writes_available: bool| ToolRuntime {
        host_settings_admitted: true,
        host_settings_saved: Default::default(),
        guard_available: true,
        git_writes_available,
        host_authentication: Default::default(),
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
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
    let catalog = |config: Config, git_writes_available: bool| {
        let context = runtime(config, git_writes_available).context().unwrap();
        parse_json(context.catalog().as_bytes(), MAX_TOOL_CATALOG_BYTES).unwrap()
    };
    assert_eq!(catalog(config.clone(), true)["git"], "unavailable");
    config.git = Some(
        serde_json::from_value(json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]})).unwrap(),
    );
    assert_eq!(
        catalog(config.clone(), true)["git"]["operations"],
        json!(["git_status", "git_diff", "git_log", "git_branch"])
    );
    config.write = true;
    assert_eq!(
        config.allows(&ToolCall::GitBranch {
            name: Some("topic".into()),
            start_point: None,
            scope: None,
            base: None,
            max_count: None,
        }),
        super::guard::git_writes_available()
    );
    for git_writes in [false, true] {
        let operations = if git_writes {
            vec![
                "git_status",
                "git_diff",
                "git_log",
                "git_commit",
                "git_branch",
                "git_switch",
            ]
        } else {
            vec!["git_status", "git_diff", "git_log", "git_branch"]
        };
        let catalog = catalog(config.clone(), git_writes);
        assert_eq!(catalog["git"]["write"], git_writes);
        assert_eq!(catalog["git"]["operations"], json!(operations));
        let branch = catalog_call_schemas(&catalog)
            .into_iter()
            .find(|call| call["properties"]["operation"]["enum"][0] == "git_branch")
            .unwrap();
        let expected = if git_writes {
            json!(["string", "null"])
        } else {
            json!("null")
        };
        for field in ["name", "start_point"] {
            assert_eq!(branch["properties"][field]["type"], expected);
        }
    }
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
        ToolCall::GitBranch {
            name: None,
            start_point: None,
            scope: Some("all".into()),
            base: Some("main".into()),
            max_count: Some(256),
        },
    ] {
        assert!(call.is_goal_plan_prelude());
        assert!(!call.is_child_research(), "{call:?}");
    }
    for call in [
        ToolCall::List { path: ".".into() },
        ToolCall::Read {
            path: "a".into(),
            offset: None,
            limit: Some(1),
        },
        ToolCall::Search {
            path: ".".into(),
            query: "x".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
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
        host_settings_admitted: true,
        host_settings_saved: Default::default(),
        guard_available: true,
        git_writes_available: true,
        host_authentication: Default::default(),
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
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
    let render = |catalog: &str| {
        let mut value: Value = serde_json::from_str(catalog).unwrap();
        value["host_clock"]["unix_ms"] = json!(0);
        value
    };
    let rendered = json!({
        "primary": render(runtime.context().unwrap().catalog()),
        "researcher": render(runtime.research_context(3).unwrap().catalog()),
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
        host_settings_admitted: true,
        host_settings_saved: Default::default(),
        guard_available: true,
        git_writes_available: false,
        host_authentication: Default::default(),
        worker: None,
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        config: serde_json::from_value(config.clone()).unwrap(),
        workspace: workspace.to_owned(),
        workspace_identity: super::fs::workspace_identity(&root).unwrap(),
        reads: Some(super::InProcessReads {
            skills: serde_json::from_value::<Config>(config.clone())
                .unwrap()
                .skills
                .iter()
                .map(|skill| {
                    (
                        skill.name.clone(),
                        super::fs::open_directory(std::path::Path::new(&skill.directory))
                            .ok()
                            .map(std::sync::Arc::new),
                    )
                })
                .collect(),
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
fn old_host_rules_are_inactive_and_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let bytes = b"malformed inactive legacy rules";
    state
        .record(crate::store::RecordKind::UnguardedCommands)
        .replace(bytes)
        .unwrap();
    let mut runtime = in_process_runtime(&workspace, config_value());
    runtime.unguarded_rules = super::ToolRuntime::load_exact_command_rules(&state).unwrap();
    assert!(runtime.has_inactive_host_rules());
    assert_eq!(runtime.receipt().unguarded_rules_digest, None);
    assert!(
        state
            .record(crate::store::RecordKind::UnguardedCommands)
            .read()
            .unwrap()
            .is_none()
    );
    let archive = state
        .path()
        .join("archive/inactive-host-commands/unguarded-commands.json");
    assert_eq!(std::fs::read(&archive).unwrap(), bytes);
    drop(state);
    let reopened = crate::store::StateRoot::open_existing(&temp.path().join("state")).unwrap();
    runtime.unguarded_rules = super::ToolRuntime::load_exact_command_rules(&reopened).unwrap();
    assert!(runtime.has_inactive_host_rules());
    assert_eq!(runtime.receipt().unguarded_rules_digest, None);
    assert!(
        reopened
            .record(crate::store::RecordKind::UnguardedCommands)
            .read()
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(archive).unwrap(), bytes);
}

async fn inspect_read(runtime: &super::ToolRuntime, intent: EffectIntent) -> ToolObservation {
    if !runtime.guard_available
        || !runtime.config.allows(&intent.call)
        || !super::discovery::possible(&intent.call)
    {
        return runtime.execute(intent, super::Cancellation::new()).await;
    }
    let root = match runtime.pinned_root(&intent) {
        Ok(root) => root,
        Err(error) => return super::unstarted_observation(intent, error),
    };
    if super::discovery::exact_search(&root, &intent.call).unwrap_or(true) {
        return runtime.execute(intent, super::Cancellation::new()).await;
    }
    let hidden = runtime.hidden_paths(&intent.call);
    let result = super::discovery::inspect(super::discovery::Request {
        root: &root,
        call: &intent.call,
        config: &runtime.config,
        hidden: &|path| hidden.covers(path),
        result_bytes: intent.limits.result_bytes as usize,
        deadline: std::time::Instant::now() + super::selection::DISCOVERY_TIME,
        output: super::discovery::Output::Tool,
        private_image: false,
        cancellation: None,
    });
    let (disposition, output, enforcement_cause, limit_cause) =
        super::dispatch::outcome(&intent, result);
    ToolObservation {
        attestation: Some(Attestation::Guard(super::GuardReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        })),
        intent,
        disposition,
        output,
        enforcement_cause,
        limit_cause,
        approval_cause: None,
        cache_update: super::CacheUpdate::NotApplicable,
    }
}

async fn run_in_process(runtime: &super::ToolRuntime, call: ToolCall) -> ToolObservation {
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call,
    );
    inspect_read(runtime, intent).await
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
        offset: None,
        limit: None,
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
        "1\tfn main() {}\n"
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

    assert_eq!(
        runtime.reservation_overhead(&ToolCall::Command {
            program: "shell".into(),
            args: vec![],
            cwd: "".into(),
        }),
        runtime.reservation_overhead(&ToolCall::Mkdir {
            path: "src/new".into()
        }) + std::time::Duration::from_secs(5),
    );

    for pin in ["grant", "enforcement"] {
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            read("src/main.rs"),
        );
        let presented = intent.clone();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        tokio::time::resume();
        let mut changed = runtime.clone();
        if pin == "grant" {
            changed.config.write = true;
        } else {
            changed.enforcement_digest = [9; 32];
        }
        let refused = changed.execute(intent, super::Cancellation::new()).await;
        assert_eq!(
            refused.intent, presented,
            "original approval pins stay fixed"
        );
        assert_eq!(refused.disposition, ToolDisposition::Failed, "{pin}");
        assert!(refused.attestation.is_none(), "{pin}");
        assert!(!refused.output.contains("fn main"), "{pin}");
    }

    let secret = run_in_process(&runtime, read("src/.env")).await;
    assert_eq!(secret.disposition, ToolDisposition::Denied);
    assert!(secret.attestation.is_none());
    for listing in [
        ToolCall::List { path: "src".into() },
        ToolCall::Search {
            path: "src".into(),
            query: "canary".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
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

    let mut invalid_intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        read("src/main.rs"),
    );
    let control = runtime
        .execute(invalid_intent.clone(), super::Cancellation::new())
        .await;
    assert_eq!(control.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        control.attestation,
        Some(Attestation::InProcess { .. })
    ));
    invalid_intent.use_count = 2;
    let invalid = runtime
        .execute(invalid_intent, super::Cancellation::new())
        .await;
    assert_eq!(invalid.disposition, ToolDisposition::Failed);
    assert!(invalid.attestation.is_none());

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
}

#[tokio::test]
async fn exact_search_path_refusals_are_attested_without_stopping_the_run() {
    let fixture = tempfile::tempdir().unwrap();
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(fixture.path().join("outside.txt"), b"outside payload").unwrap();
    std::os::unix::fs::symlink(fixture.path().join("outside.txt"), workspace.join("link")).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (path, disposition) in [
        ("link", ToolDisposition::Denied),
        ("missing", ToolDisposition::Failed),
    ] {
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            search(path, "payload"),
        );
        let inspected = inspect_read(&runtime, intent.clone()).await;
        let observation = runtime.execute(intent, super::Cancellation::new()).await;
        assert_eq!(inspected, observation);
        assert_eq!(observation.disposition, disposition);
        assert!(observation.valid() && !observation.stops_run());
        assert!(matches!(
            observation.attestation,
            Some(Attestation::InProcess { .. })
        ));
        assert!(!observation.output.contains("outside payload"));
    }
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        search("missing", "payload"),
    );
    std::fs::rename(&workspace, fixture.path().join("moved")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let observation = runtime.execute(intent, super::Cancellation::new()).await;
    assert!(observation.stops_run() && observation.attestation.is_none());
    assert!(observation.output.contains("pinned input"));
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
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
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
        let observation = inspect_read(&runtime, intent).await;
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
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
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
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{call:?} with no matches"
        );
        let rows: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(rows["entries"], json!([]));
    }
}

fn search(path: &str, query: &str) -> ToolCall {
    ToolCall::Search {
        path: path.into(),
        query: query.into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset: 0,
        limit: MAX_SEARCH_ROWS,
    }
}

fn with_search(
    mut call: ToolCall,
    edit: impl FnOnce(&mut bool, &mut Option<String>, &mut bool, &mut u32, &mut u32),
) -> ToolCall {
    if let ToolCall::Search {
        regex,
        include,
        case_sensitive,
        offset,
        limit,
        ..
    } = &mut call
    {
        edit(regex, include, case_sensitive, offset, limit);
    }
    call
}

async fn search_page(runtime: &super::ToolRuntime, call: ToolCall, result_bytes: u32) -> Value {
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call.clone(),
    );
    intent.limits.result_bytes = result_bytes;
    let observation = inspect_read(runtime, intent).await;
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{call:?}: {}",
        observation.output
    );
    assert!(observation.output.len() <= result_bytes as usize);
    serde_json::from_str(&observation.output).unwrap()
}

fn rows(page: &Value) -> Vec<(String, u64)> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap().to_owned(),
                row["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn search_matches_regex_include_and_case_in_byte_order_without_protected_files() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for (path, text) in [
        ("src/nested/c.rs", "fn helper() {}\nfoo()\n"),
        ("src/a/z.rs", "foo\n"),
        ("src/b.txt", "foo\nFOO\n"),
        ("src/a.rs", "fn main() {}\nlet Foo = 1;\n// foo bar\n"),
        ("src/.env", "foo secret-canary\n"),
        ("src/.env.local", "foo secret-canary\n"),
        ("src/.ssh/key", "foo secret-canary\n"),
        ("target/build.rs", "foo ignored-canary\n"),
    ] {
        std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let row = |path: &str, line: u64| (path.to_owned(), line);
    let regex = |call| with_search(call, |regex, _, _, _, _| *regex = true);
    let include =
        |call, glob: &str| with_search(call, |_, include, _, _, _| *include = Some(glob.into()));
    let folded = |call| with_search(call, |_, _, case, _, _| *case = false);
    for (call, expected) in [
        (
            search("src", "foo"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            folded(search("src", "foo")),
            vec![
                row("src/a.rs", 2),
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/b.txt", 2),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            search("src", "("),
            vec![
                row("src/a.rs", 1),
                row("src/nested/c.rs", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            regex(search("src", r"^fn \w+\(")),
            vec![row("src/a.rs", 1), row("src/nested/c.rs", 1)],
        ),
        (
            folded(regex(search(".", "^FOO$"))),
            vec![
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/b.txt", 2),
            ],
        ),
        (
            include(search("src", "foo"), "*.rs"),
            vec![row("src/a.rs", 3)],
        ),
        (
            include(search("src", "foo"), "**/*.rs"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
        (
            include(search(".", "foo"), "src/{b.txt,a.?s}"),
            vec![row("src/a.rs", 3), row("src/b.txt", 1)],
        ),
        (
            include(search("src/b.txt", "foo"), "b.txt"),
            vec![row("src/b.txt", 1)],
        ),
        (include(search("src/b.txt", "foo"), "*.rs"), vec![]),
        (include(search(".", "foo"), "**/.env"), vec![]),
        (
            search(".", "foo"),
            vec![
                row("src/a.rs", 3),
                row("src/a/z.rs", 1),
                row("src/b.txt", 1),
                row("src/nested/c.rs", 2),
            ],
        ),
    ] {
        let page = search_page(&runtime, call.clone(), MAX_TOOL_RESULT_BYTES as u32).await;
        assert_eq!(rows(&page), expected, "{call:?}");
        assert_eq!(page["next_offset"], Value::Null, "{call:?}");
        assert_eq!(page["may_be_truncated"], false, "{call:?}");
        assert!(!page.to_string().contains("canary"), "{call:?}");
    }

    let large = r"\w{1000}";
    for (call, cause) in [
        (regex(search("src", "(")), "invalid or unsupported regex"),
        (
            regex(search("src", "(?=foo)")),
            "invalid or unsupported regex",
        ),
        (
            regex(search("src", r"(f)\1")),
            "invalid or unsupported regex",
        ),
        (regex(search("src", large)), "regex exceeds the 128 KiB"),
        (
            include(search("src", "foo"), "[ab].rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "a**/b"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,{b}}"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,}"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "/src/*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "../*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "!*.rs"),
            "include glob is invalid",
        ),
        (
            include(search("src", "foo"), "{a,b,c,d,e,f,g,h}{i,j,k,l,m,n,o,p,q}"),
            "include glob exceeds",
        ),
    ] {
        assert!(call.valid(), "{call:?}");
        // Refusals fit the smallest admitted result budget.
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = 128;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(observation.disposition, ToolDisposition::Failed, "{call:?}");
        assert!(
            observation.output.starts_with("Search pattern refused: ")
                && observation.output.contains(cause)
                && observation.output.ends_with("; nothing was read"),
            "{call:?}: {}",
            observation.output
        );
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Guard(_))
        ));
        assert!(!observation.stops_run());
        assert!(observation.valid());
    }
}

#[tokio::test]
async fn search_pages_rows_with_a_hard_cap_and_resumes_after_fitted_rows() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    // Shuffled creation order; pages follow path byte order, not directory order.
    let mut expected = Vec::new();
    for index in [3, 0, 4, 1, 2] {
        let text: String = (1..=61)
            .map(|line| format!("hit {index}.{line}\nmiss\n"))
            .collect();
        std::fs::write(workspace.join(format!("src/f{index}.txt")), text).unwrap();
    }
    for index in 0..5 {
        expected.extend((1..=61).map(|line| (format!("src/f{index}.txt"), 2 * line - 1)));
    }
    std::fs::write(
        workspace.join("src/exact.md"),
        "exact\n".repeat(MAX_SEARCH_ROWS as usize),
    )
    .unwrap();
    std::fs::write(
        workspace.join("src/over.md"),
        "over\n".repeat(MAX_SEARCH_ROWS as usize + 1),
    )
    .unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let paged = |offset: u32, limit: u32| {
        with_search(search("src", "hit"), move |_, _, _, o, l| {
            (*o, *l) = (offset, limit);
        })
    };

    for (budget, limit) in [
        (MAX_TOOL_RESULT_BYTES as u32, MAX_SEARCH_ROWS),
        (1024, MAX_SEARCH_ROWS),
        (MAX_TOOL_RESULT_BYTES as u32, 7),
    ] {
        let (mut seen, mut offset, mut pages) = (Vec::new(), Some(0), 0);
        while let Some(start) = offset {
            let page = search_page(&runtime, paged(start, limit), budget).await;
            let page_rows = rows(&page);
            assert!(!page_rows.is_empty() && page_rows.len() <= limit as usize);
            assert_eq!(page["limit"], limit);
            offset = page["next_offset"].as_u64().map(|next| next as u32);
            assert_eq!(page["may_be_truncated"], offset.is_some());
            if let Some(next) = offset {
                assert_eq!(next as usize, start as usize + page_rows.len());
            }
            seen.extend(page_rows);
            pages += 1;
        }
        assert_eq!(seen, expected, "budget {budget}, limit {limit}");
        assert!(pages >= expected.len().div_ceil(limit as usize));
    }
    let first = search_page(
        &runtime,
        paged(0, MAX_SEARCH_ROWS),
        MAX_TOOL_RESULT_BYTES as u32,
    )
    .await;
    assert_eq!(rows(&first).len(), MAX_SEARCH_ROWS as usize);
    assert_eq!(first["next_offset"], MAX_SEARCH_ROWS);

    for (query, count, next) in [
        ("exact", MAX_SEARCH_ROWS, Value::Null),
        ("over", MAX_SEARCH_ROWS, json!(MAX_SEARCH_ROWS)),
        ("absent", 0, Value::Null),
    ] {
        let page = search_page(&runtime, search("src", query), MAX_TOOL_RESULT_BYTES as u32).await;
        assert_eq!(rows(&page).len(), count as usize, "{query}");
        assert_eq!(page["next_offset"], next, "{query}");
    }
    let past = search_page(&runtime, paged(10_000, MAX_SEARCH_ROWS), 1024).await;
    assert_eq!(past["entries"], json!([]));
    assert_eq!(past["next_offset"], Value::Null);

    for call in [
        paged(0, 0),
        paged(0, MAX_SEARCH_ROWS + 1),
        paged(MAX_SNAPSHOT_BYTES as u32 + 1, 1),
        with_search(search("src", "hit"), |_, include, _, _, _| {
            *include = Some(String::new());
        }),
    ] {
        assert!(!call.valid(), "{call:?}");
    }
    assert!(paged(MAX_SNAPSHOT_BYTES as u32, MAX_SEARCH_ROWS).valid());
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
    let reserve = super::ToolRuntime::step_context_reserve(false, 0);
    let fitted = |overhead: usize| limit.saturating_sub(overhead + 512);
    let ceiling = |overhead: usize| {
        runtime
            .result_capacity(overhead, reserve)
            .map(|capacity| capacity.min(MAX_TOOL_RESULT_BYTES))
    };
    let below_ceiling = limit - 512 - (MAX_TOOL_RESULT_BYTES / 2);
    assert_eq!(
        runtime.result_capacity(below_ceiling, reserve),
        Some(fitted(below_ceiling))
    );
    assert_eq!(ceiling(below_ceiling), Some(MAX_TOOL_RESULT_BYTES / 2));
    assert!(
        fitted(0) > MAX_TOOL_RESULT_BYTES,
        "the context fit exceeds the ceiling at zero overhead"
    );
    assert_eq!(ceiling(0), Some(MAX_TOOL_RESULT_BYTES));
    let at_floor = limit - 512 - 128;
    assert_eq!(runtime.result_capacity(at_floor, reserve), Some(128));
    assert_eq!(runtime.result_capacity(at_floor + 1, reserve), None);
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
                offset: None,
                limit: None,
            },
        );
        intent.limits.result_bytes = result_bytes;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.output.len() <= result_bytes as usize);
        let page: Value = serde_json::from_str(&observation.output).unwrap();
        let next = page["next_line"].as_u64().unwrap() as usize;
        assert!(next > 0 && next < text.len());
        assert_eq!(page["truncated"], true);
        assert_eq!(
            page["text"],
            (1..next)
                .map(|n| format!("{n}\t\"quoted\\line\"\n"))
                .collect::<String>()
        );
    }
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Search {
            path: "src".into(),
            query: "quoted".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
    );
    intent.limits.result_bytes = 1024;
    let observation = inspect_read(&runtime, intent).await;
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
            offset: None,
            limit: Some(1),
        },
    );
    intent.limits.result_bytes = 128;
    let observation = inspect_read(&runtime, intent).await;
    assert_eq!(observation.disposition, ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(super::LimitCause::Resource));
}

#[test]
fn the_primary_prompt_states_the_read_page_limit() {
    assert!(
        include_str!("../provider/prompts/primary.txt").contains(&format!(
            "a nullable line limit from 1 to {MAX_READ_PAGE_LINES},"
        ))
    );
}

#[tokio::test]
async fn in_process_repeated_reads_all_succeed_on_a_large_file() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n".repeat(4096)).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let mut elapsed = [Duration::ZERO; 40];
    for sample in &mut elapsed {
        let start = Instant::now();
        let observation = run_in_process(
            &runtime,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: None,
            },
        )
        .await;
        *sample = start.elapsed();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 2000);
        assert_eq!(result["next_line"], 2001);
        assert_eq!(result["text"].as_str().unwrap().lines().count(), 2000);
    }
    elapsed.sort_unstable();
    eprintln!(
        "Read latency: 40 sequential reads of a freshly written 53248-byte file; p50={:?}, p95={:?}, max={:?}",
        elapsed[19], elapsed[37], elapsed[39]
    );
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
            offset: None,
            limit: Some(16),
        },
    )
    .await;
    drop(held);
    assert_eq!(refused.disposition, ToolDisposition::Limit);
    assert_eq!(refused.limit_cause, Some(super::LimitCause::ReadCapacity));
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

#[test]
fn guard_refuses_a_forged_cache_generation_after_the_host_check() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut runtime = in_process_runtime(&workspace, config_value());
    runtime.cache_state = Some(std::sync::Arc::new(state));
    let mut intent = runtime.intent(
        crate::RunId::new(),
        crate::AgentRunId::new(),
        ToolCall::Command {
            program: "sh".into(),
            args: vec![],
            cwd: ".".into(),
        },
    );
    intent.target = crate::session::worker::EffectTarget::WorkerView {
        assignment_id: intent.agent_run_id,
        base_digest: [1; 32],
        revision: 0,
        cache_generation: Some([9; 32]),
    };
    intent.limits.cache_bytes = 1 << 20;
    assert!(matches!(
        super::guard::build_cache_session(&runtime, &intent),
        Err(super::ToolError::ChangedInput)
    ));
    if let crate::session::worker::EffectTarget::WorkerView {
        cache_generation, ..
    } = &mut intent.target
    {
        *cache_generation = None;
    }
    assert!(
        super::guard::build_cache_session(&runtime, &intent)
            .unwrap()
            .is_some()
    );
}

fn glob(pattern: &str, path: &str, limit: u32) -> ToolCall {
    ToolCall::Glob {
        pattern: pattern.into(),
        path: path.into(),
        limit,
    }
}

async fn glob_result(
    runtime: &super::ToolRuntime,
    call: ToolCall,
    result_bytes: u32,
) -> ToolObservation {
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call,
    );
    intent.limits.result_bytes = result_bytes;
    let observation = inspect_read(runtime, intent).await;
    assert!(observation.valid());
    observation
}

fn glob_paths(observation: &ToolObservation) -> (Vec<String>, bool) {
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{}",
        observation.output
    );
    assert!(matches!(
        observation.attestation,
        Some(Attestation::Guard(_))
    ));
    let value: Value = serde_json::from_str(&observation.output).unwrap();
    let paths = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect();
    (paths, value["may_be_truncated"].as_bool().unwrap())
}

#[tokio::test]
async fn glob_returns_admitted_regular_files_in_path_order_with_honest_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for directory in ["src/deep", ".github", "target", "node_modules", "many"] {
        std::fs::create_dir_all(workspace.join(directory)).unwrap();
    }
    for (path, content) in [
        ("Cargo.toml", "[package]"),
        ("README.md", "readme"),
        (".gitignore", "target"),
        (".github/ci.yml", "on: push"),
        ("src/main.rs", "fn main() {}"),
        ("src/lib.rs", ""),
        ("src/deep/mod.rs", "mod deep;"),
        ("src/v1.txt", "v1"),
        ("src/v12.txt", "v12"),
        ("target/build.rs", "excluded"),
        ("node_modules/dep.rs", "excluded"),
    ] {
        std::fs::write(workspace.join(path), content).unwrap();
    }
    // Sizes come from metadata alone, beyond the 1 MiB content collector.
    std::fs::File::create(workspace.join("big.bin"))
        .unwrap()
        .set_len(2 * 1024 * 1024)
        .unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, expected, truncated) in [
        (
            glob("**/*.rs", ".", 100),
            &["src/deep/mod.rs", "src/lib.rs", "src/main.rs"][..],
            false,
        ),
        (
            glob("src/**", ".", 100),
            &[
                "src/deep/mod.rs",
                "src/lib.rs",
                "src/main.rs",
                "src/v1.txt",
                "src/v12.txt",
            ],
            false,
        ),
        (
            glob("*.{toml,md}", ".", 100),
            &["Cargo.toml", "README.md"],
            false,
        ),
        (glob("src/v?.txt", ".", 100), &["src/v1.txt"], false),
        (glob(".*", ".", 100), &[".gitignore"], false),
        (glob("**/*.yml", ".", 100), &[".github/ci.yml"], false),
        (
            glob("*.rs", "src", 100),
            &["src/lib.rs", "src/main.rs"],
            false,
        ),
        (
            glob("**/*.rs", ".", 2),
            &["src/deep/mod.rs", "src/lib.rs"],
            true,
        ),
        (glob("*.none", ".", 100), &[], false),
        (glob("target/*", ".", 100), &[], false),
    ] {
        let observation = glob_result(&runtime, call.clone(), 64 * 1024).await;
        assert_eq!(
            glob_paths(&observation),
            (
                expected.iter().map(|p| (*p).to_owned()).collect(),
                truncated
            ),
            "{call:?}"
        );
    }
    let big = glob_result(&runtime, glob("big.*", ".", 100), 1024).await;
    assert_eq!(
        serde_json::from_str::<Value>(&big.output).unwrap(),
        json!({"entries":[{"path":"big.bin","bytes":2 * 1024 * 1024}],"limit":100,"may_be_truncated":false})
    );

    let mut created = Vec::new();
    for (count, truncated) in [(99, false), (100, false), (101, true)] {
        // A fixed permutation keeps directory insertion order unrelated to path order.
        while created.len() < count {
            let name = format!("many/f{:03}.txt", (created.len() * 37) % 101);
            std::fs::write(workspace.join(&name), "x").unwrap();
            created.push(name);
        }
        let mut sorted = created.clone();
        sorted.sort();
        sorted.truncate(100);
        let observation = glob_result(&runtime, glob("many/*.txt", ".", 100), 64 * 1024).await;
        assert_eq!(
            glob_paths(&observation),
            (sorted.clone(), truncated),
            "{count}"
        );
        let small = glob_result(&runtime, glob("many/*.txt", ".", 100), 512).await;
        let (fitted, fitted_truncated) = glob_paths(&small);
        assert!(small.output.len() <= 512);
        assert!(fitted_truncated && !fitted.is_empty());
        assert_eq!(fitted[..], sorted[..fitted.len()]);
    }
    let long = format!("{}.txt", "a".repeat(160));
    std::fs::write(workspace.join("src").join(&long), "").unwrap();
    let unfit = glob_result(&runtime, glob("src/a*.txt", ".", 100), 128).await;
    assert_eq!(unfit.disposition, ToolDisposition::Limit);
    assert_eq!(unfit.limit_cause, Some(super::LimitCause::Resource));
}

#[tokio::test]
async fn glob_never_discloses_protected_or_unadmitted_entries_and_bounds_its_walk() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for directory in ["src", ".git", ".ssh", "outside"] {
        std::fs::create_dir_all(workspace.join(directory)).unwrap();
    }
    std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
    for (path, content) in [
        ("src/main.rs", "fn main() {}"),
        ("src/hard_a.rs", "hard"),
        (".env", "TOKEN=glob-canary"),
        ("src/.env.local", "TOKEN=glob-canary"),
        ("src/.ENV", "TOKEN=glob-canary"),
        (".git/config.rs", "glob-canary"),
        (".ssh/id.rs", "glob-canary"),
        ("Cargo.toml", "[package]"),
    ] {
        std::fs::write(workspace.join(path), content).unwrap();
    }
    std::fs::hard_link(
        workspace.join("src/hard_a.rs"),
        workspace.join("src/hard_b.rs"),
    )
    .unwrap();
    std::fs::write(temp.path().join("elsewhere/escape.rs"), "glob-canary").unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("elsewhere/escape.rs"),
        workspace.join("src/link.rs"),
    )
    .unwrap();
    std::os::unix::fs::symlink(temp.path().join("elsewhere"), workspace.join("linked")).unwrap();
    let whole = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for pattern in ["**", "**/*.rs", ".*", "**/.env*", "src/*"] {
        let observation = glob_result(&whole, glob(pattern, ".", 100), 64 * 1024).await;
        let (paths, _) = glob_paths(&observation);
        assert!(
            paths
                .iter()
                .all(|path| ["Cargo.toml", "src/main.rs"].contains(&path.as_str())),
            "{pattern}: {paths:?}"
        );
        for hidden in [
            "canary", ".env", ".ENV", ".git", ".ssh", "hard_", "link", "escape",
        ] {
            assert!(!observation.output.contains(hidden), "{pattern}: {hidden}");
        }
    }
    let selected = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["Cargo.toml","src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, expected) in [
        (glob("**", ".", 100), &["Cargo.toml", "src/main.rs"][..]),
        (glob("*.rs", "src", 100), &["src/main.rs"]),
        (glob("*", ".", 100), &["Cargo.toml"]),
    ] {
        let observation = glob_result(&selected, call.clone(), 64 * 1024).await;
        assert_eq!(glob_paths(&observation).0, expected, "{call:?}");
    }
    let outside = glob_result(&selected, glob("**", "outside", 100), 1024).await;
    assert_eq!(outside.disposition, ToolDisposition::Denied);

    for (call, disposition, output) in [
        (
            glob("*", "Cargo.toml", 100),
            ToolDisposition::Failed,
            "Tool operation failed",
        ),
        (
            glob("*", "missing", 100),
            ToolDisposition::Failed,
            "File or directory not found",
        ),
        (
            glob("*", "linked", 100),
            ToolDisposition::Denied,
            "outside the admitted regular-file boundary",
        ),
    ] {
        let observation = glob_result(&whole, call.clone(), 1024).await;
        assert_eq!(observation.disposition, disposition, "{call:?}");
        assert!(
            observation.output.contains(output),
            "{}",
            observation.output
        );
        assert!(!observation.output.contains("canary"));
    }

    let deep = (0..16)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    std::fs::create_dir_all(workspace.join(&deep)).unwrap();
    std::fs::write(workspace.join(&deep).join("leaf.rs"), "").unwrap();
    let too_deep = glob_result(&whole, glob("**/*.rs", ".", 100), 1024).await;
    assert_eq!(too_deep.disposition, ToolDisposition::Limit);
    let shallow = glob_result(&whole, glob("*.toml", ".", 100), 1024).await;
    assert_eq!(glob_paths(&shallow).0, ["Cargo.toml"]);
    std::fs::remove_dir_all(workspace.join("d0")).unwrap();

    std::fs::create_dir_all(workspace.join("wide")).unwrap();
    for index in 0..=MAX_SNAPSHOT_FILES {
        std::fs::write(workspace.join(format!("wide/f{index:05}")), "").unwrap();
    }
    let candidates = glob_result(&whole, glob("wide/*", ".", 100), 1024).await;
    assert_eq!(candidates.disposition, ToolDisposition::Limit);
    let one = glob_result(&whole, glob("wide/f00000", ".", 100), 1024).await;
    assert_eq!(glob_paths(&one).0, ["wide/f00000"]);
    for index in MAX_SNAPSHOT_FILES + 1..MAX_SNAPSHOT_FILES * 4 {
        std::fs::write(workspace.join(format!("wide/f{index:05}")), "").unwrap();
    }
    let scanned = glob_result(&whole, glob("wide/f00000", ".", 100), 1024).await;
    assert_eq!(scanned.disposition, ToolDisposition::Limit);
    assert_eq!(scanned.limit_cause, Some(super::LimitCause::Resource));
    assert!(scanned.attestation.is_some());
}

#[tokio::test]
// Skipped in the Guard: local test IPC capability is missing.
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn glob_never_discloses_unix_socket_entries() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("visible.rs"), "fn main() {}").unwrap();
    let _socket =
        std::os::unix::net::UnixListener::bind(temp.path().join("socket-canary.rs")).unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for pattern in ["**", "**/*.rs", "*"] {
        let observation = glob_result(&runtime, glob(pattern, ".", 100), 1024).await;
        assert_eq!(glob_paths(&observation), (vec!["visible.rs".into()], false));
        assert!(!observation.output.contains("socket-canary"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn restricted_read_paths_stay_out_of_list_search_and_glob_results_unopened() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for (path, text) in [
        ("a.rs", "foo\n"),
        ("private/secret.rs", "foo deny-canary\n"),
        ("private/deep/x.rs", "foo deny-canary\n"),
        ("src/b.rs", "foo\n"),
        ("src/c.KEY", "foo ask-canary\n"),
        ("src/caf\u{e9}.rs", "foo unicode-canary\n"),
        ("src/z.rs", "foo\n"),
        ("vault/v.rs", "foo approved\n"),
    ] {
        std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
        std::fs::write(workspace.join(path), text).unwrap();
    }
    // Opening any of these fails, so a walk that touched one would fail instead of succeeding.
    for path in ["private/secret.rs", "src/c.KEY", "private/deep"] {
        std::fs::set_permissions(workspace.join(path), std::fs::Permissions::from_mode(0o000))
            .unwrap();
    }
    let mut runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let settings = br#"{"permissions":{"deny":["Read(./private/**)"],"ask":["Read(**/*.key)","Read(./vault/**)"]}}"#;
    runtime.permissions = Some(super::PermissionSnapshot {
        config: std::sync::Arc::new(super::permissions::PermissionConfig::parse(settings).unwrap()),
        digest: digest(settings),
    });
    let paths = |page: &Value| {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_str().or(row["path"].as_str()).unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let visible = ["a.rs", "src/b.rs", "src/z.rs"];
    let page = search_page(&runtime, search(".", "foo"), 64 * 1024).await;
    assert_eq!(paths(&page), visible, "{page}");
    for (offset, path, next) in [
        (0, "a.rs", json!(1)),
        (1, "src/b.rs", json!(2)),
        (2, "src/z.rs", Value::Null),
    ] {
        let call = with_search(search(".", "foo"), |_, _, _, start, limit| {
            (*start, *limit) = (offset, 1);
        });
        let page = search_page(&runtime, call, 64 * 1024).await;
        assert_eq!(
            (paths(&page), &page["next_offset"]),
            (vec![path.to_owned()], &next)
        );
    }
    // An approved ask covers the call's own path, so a search inside it shows its rows.
    let approved = search_page(&runtime, search("vault", "foo"), 64 * 1024).await;
    assert_eq!(paths(&approved), ["vault/v.rs"]);
    let list = |path: &str| ToolCall::List { path: path.into() };
    assert_eq!(
        paths(&search_page(&runtime, list("."), 64 * 1024).await),
        ["a.rs", "src/"]
    );
    assert_eq!(
        paths(&search_page(&runtime, list("src"), 64 * 1024).await),
        ["src/b.rs", "src/z.rs"]
    );
    let (found, truncated) =
        glob_paths(&glob_result(&runtime, glob("**", ".", 100), 64 * 1024).await);
    assert_eq!(
        (found, truncated),
        (visible.map(str::to_owned).to_vec(), false)
    );
    let (found, _) = glob_paths(&glob_result(&runtime, glob("**", ".", 1), 64 * 1024).await);
    assert_eq!(found, ["a.rs"]);
    for path in ["private/secret.rs", "src/c.KEY", "private/deep"] {
        std::fs::set_permissions(workspace.join(path), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
}

#[test]
fn read_restrictions_cover_workspace_skill_files_and_catalog_previews() {
    use super::permissions::{Decision, PermissionConfig};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for (path, text) in [
        (
            ".agents/skills/x/SKILL.md",
            "---\ndescription: shown x\n---\n",
        ),
        (".agents/skills/x/private/key", "DENY_CANARY\n"),
        (
            ".agents/skills/y/SKILL.md",
            "---\ndescription: ASK_CANARY\n---\n",
        ),
    ] {
        std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let mut runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.config.discover_skills(&workspace).unwrap();
    let settings = br#"{"permissions":{"deny":["Read(./.agents/skills/x/private/**)"],"ask":["Read(./.agents/skills/y/**)"]}}"#;
    runtime.permissions = Some(super::PermissionSnapshot {
        config: std::sync::Arc::new(PermissionConfig::parse(settings).unwrap()),
        digest: digest(settings),
    });
    let config = runtime.permissions().unwrap();
    let skill = |name: &str, resource: Option<&str>| ToolCall::Skill {
        name: name.into(),
        resource: resource.map(str::to_owned),
    };
    let decide = |call: &ToolCall| {
        let (decision, _, approval) =
            config.evaluate_skill_file(call, runtime.skill_workspace_path(call).as_deref());
        (decision, approval)
    };
    let unrestricted = config.evaluate(&skill("x", None));
    assert_eq!(decide(&skill("x", None)), (unrestricted.0, unrestricted.2));
    assert_eq!(
        decide(&skill("x", Some("private/key"))),
        (Decision::Deny, None)
    );
    assert_eq!(
        decide(&skill("y", None)),
        (Decision::Ask, Some(super::ApprovalMode::Request))
    );
    let catalog = runtime.context().unwrap().catalog().to_owned();
    assert!(
        catalog.contains("shown x") && !catalog.contains("ASK_CANARY"),
        "{catalog}"
    );
    assert!(!catalog.contains("\"y\""), "{catalog}");
}

#[test]
fn a_workspace_root_skill_maps_to_a_relative_path_and_stays_cataloged() {
    use super::permissions::PermissionConfig;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.config.skills.push(super::config::Skill {
        disable_model_invocation: false,
        name: "root".into(),
        description: "root skill".into(),
        directory: workspace.to_str().unwrap().to_owned(),
        files: std::collections::BTreeMap::from([("SKILL.md".into(), "0".repeat(64))]),
    });
    runtime.permissions = Some(super::PermissionSnapshot {
        config: std::sync::Arc::new(PermissionConfig::parse(b"{}").unwrap()),
        digest: digest(b"{}"),
    });
    let call = ToolCall::Skill {
        name: "root".into(),
        resource: None,
    };
    assert_eq!(
        runtime.skill_workspace_path(&call).as_deref(),
        Some("SKILL.md")
    );
    let config = runtime.permissions().unwrap();
    assert_eq!(
        config.evaluate_skill_file(&call, runtime.skill_workspace_path(&call).as_deref()),
        config.evaluate(&call)
    );
    assert!(runtime.context().unwrap().catalog().contains("root skill"));
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
            std::time::Instant::now() + super::selection::PHASE_TIME,
        )
        .unwrap();
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
            std::time::Instant::now() + super::selection::PHASE_TIME,
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
                    std::time::Instant::now() + super::selection::PHASE_TIME,
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
            std::time::Instant::now() + super::selection::PHASE_TIME,
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
            std::time::Instant::now() + super::selection::PHASE_TIME,
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
            std::time::Instant::now() + super::selection::PHASE_TIME,
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
        std::time::Instant::now() + super::selection::PHASE_TIME,
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

#[tokio::test]
async fn unavailable_guard_hides_non_host_tools_and_refuses_before_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = in_process_runtime(temp.path(), config_value());
    runtime.config.skills.push(super::config::Skill {
        disable_model_invocation: false,
        name: "root".into(),
        description: "root skill".into(),
        directory: temp.path().to_str().unwrap().to_owned(),
        files: std::collections::BTreeMap::from([("SKILL.md".into(), "0".repeat(64))]),
    });
    assert!(runtime.allows_skill("root"));
    assert!(runtime.context().unwrap().catalog().contains("root skill"));
    runtime.guard_available = false;
    assert!(!runtime.allows_skill("root"));
    let context: serde_json::Value =
        serde_json::from_str(runtime.context().unwrap().catalog()).unwrap();
    assert_eq!(context["commands"], serde_json::json!([]));
    assert_eq!(context["mcp_servers"], serde_json::json!([]));
    assert_eq!(context["file_tools"], "");
    assert_eq!(context["skills"], serde_json::json!([]));
    assert_eq!(context["host_commands"], true);
    assert_eq!(context["write"], false);
    let calls = catalog_call_schemas(&context);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["properties"]["operation"]["enum"],
        json!(["command_unguarded"])
    );
    let research: Value =
        serde_json::from_str(runtime.research_context(3).unwrap().catalog()).unwrap();
    assert!(catalog_call_schemas(&research).is_empty());
    assert!(runtime.worker_policy().is_none());
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/file.txt"), "file canary").unwrap();
    for (call, site) in [
        (
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "touch canary".into()],
                cwd: String::new(),
            },
            "Guard",
        ),
        (ToolCall::List { path: "src".into() }, "Read"),
        (
            ToolCall::Read {
                path: "src/file.txt".into(),
                offset: None,
                limit: None,
            },
            "Read",
        ),
        (
            ToolCall::Search {
                path: "src".into(),
                query: "canary".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            "Read",
        ),
        (
            ToolCall::Glob {
                pattern: "src/*".into(),
                path: ".".into(),
                limit: 100,
            },
            "Read",
        ),
        (
            ToolCall::Skill {
                name: "root".into(),
                resource: None,
            },
            "Read",
        ),
    ] {
        let result = run_in_process(&runtime, call).await;
        assert!(result.attestation.is_none());
        assert_eq!(result.disposition, ToolDisposition::Failed);
        let mut expected = format!(
            "{site} native protection unavailable before dispatch; no operation was dispatched."
        );
        if site == "Guard" {
            expected.push_str(" Cause: native protection. Check the Guard executable and native sandbox prerequisites, then resend your message.");
            if cfg!(debug_assertions) {
                expected.push_str(
                    " For details, inspect development.log in the Session State directory.",
                );
            }
        }
        assert_eq!(result.output, expected);
    }
    assert!(!temp.path().join("canary").exists());
}

#[test]
fn host_pin_requires_explicit_nullable_permissions_digest() {
    let pin = super::types::test_host_pin();
    let mut value = serde_json::to_value(&pin).unwrap();
    assert!(value["permissions_digest"].is_null());
    assert_eq!(
        serde_json::from_value::<super::types::HostCommandPin>(value.clone()).unwrap(),
        pin
    );
    value.as_object_mut().unwrap().remove("permissions_digest");
    assert!(serde_json::from_value::<super::types::HostCommandPin>(value).is_err());
}

#[tokio::test]
async fn an_unavailable_skill_root_does_not_disable_other_tools_or_renew_its_pin() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let good = temp.path().join("good-skill");
    let unavailable = temp.path().join("missing-skill");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&good).unwrap();
    std::fs::write(workspace.join("note.txt"), "workspace fact").unwrap();
    std::fs::write(good.join("SKILL.md"), "good instructions").unwrap();
    let config = serde_json::from_value(json!({
        "version":1,"workspace_paths":["."],"write":false,"commands":[],"mcp":[],
        "skills":[
            {"name":"good","description":"available","directory":good,"files":{"SKILL.md":hex_digest(b"good instructions")}},
            {"name":"missing","description":"unavailable","directory":unavailable,"files":{"SKILL.md":hex_digest(b"late instructions")}}
        ]
    })).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let runtime = super::ToolRuntime::admit(
        config,
        None,
        None,
        &workspace,
        &state,
        temp.path().join("missing-guard"),
    )
    .expect("unrelated Tools remain available");
    assert_eq!(
        run_in_process(
            &runtime,
            ToolCall::Read {
                path: "note.txt".into(),
                offset: None,
                limit: None
            }
        )
        .await
        .disposition,
        ToolDisposition::Succeeded
    );
    assert_eq!(
        run_in_process(
            &runtime,
            ToolCall::Skill {
                name: "good".into(),
                resource: None
            }
        )
        .await
        .output,
        "good instructions"
    );
    let missing = ToolCall::Skill {
        name: "missing".into(),
        resource: None,
    };
    assert_eq!(
        run_in_process(&runtime, missing.clone()).await.disposition,
        ToolDisposition::Failed
    );
    std::fs::create_dir(&unavailable).unwrap();
    std::fs::write(unavailable.join("SKILL.md"), "late instructions").unwrap();
    assert_eq!(
        run_in_process(&runtime, missing).await.disposition,
        ToolDisposition::Failed
    );
}

#[tokio::test]
async fn skill_text_reads_are_bounded_inert_and_pinned_without_guard_execution() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let directory = temp.path().join("skills/review");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(directory.join("references")).unwrap();
    let body = format!(
        "---\nname: review\ndescription: !execute touch marker\n---\n{}",
        "é\n\"\u{1}".repeat(2000)
    );
    std::fs::write(directory.join("SKILL.md"), &body).unwrap();
    std::fs::write(directory.join("references/note.txt"), "reference").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({
            "version":1,"workspace_paths":["."],"write":false,"commands":[],"mcp":[],
            "skills":[{"name":"review","description":"Reviewed guidance","directory":directory,
                "files":{"SKILL.md":hex_digest(body.as_bytes()),"references/note.txt":hex_digest(b"reference")}}]
        }),
    );
    let call = ToolCall::Skill {
        name: "review".into(),
        resource: None,
    };
    for limit in [128, 1024, MAX_TOOL_RESULT_BYTES] {
        let mut intent =
            runtime.intent(crate::RunId::new(), crate::AgentRunId::new(), call.clone());
        intent.limits.result_bytes = limit as u32;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(matches!(
            observation.attestation,
            Some(Attestation::InProcess { .. })
        ));
        assert!(serde_json::to_string(&observation.output).unwrap().len() <= limit);
        assert_eq!(
            observation.output.contains(" lines omitted;"),
            limit < MAX_TOOL_RESULT_BYTES
        );
        assert!(observation.valid());
        let mut guarded = observation.clone();
        guarded.attestation = Some(Attestation::Guard(GuardReceipt {
            contract_version: 1,
            intent_digest: guarded.intent.digest(),
            enforcement_digest: guarded.intent.enforcement_digest,
            limits: guarded.intent.limits.clone(),
        }));
        assert!(!guarded.valid(), "Skill reads cannot carry Guard receipts");
        guarded.attestation = None;
        guarded.disposition = ToolDisposition::Failed;
        guarded.enforcement_cause = Some(EnforcementCause::GuardBootstrap);
        assert!(
            !guarded.valid(),
            "Skill reads cannot claim Guard bootstrap failure"
        );
    }
    assert!(!workspace.join("marker").exists());
    let reference = ToolCall::Skill {
        name: "review".into(),
        resource: Some("references/note.txt".into()),
    };
    assert_eq!(
        run_in_process(&runtime, reference.clone()).await.output,
        "reference"
    );
    std::fs::write(directory.join("SKILL.md"), "changed after admission").unwrap();
    assert_ne!(
        run_in_process(&runtime, call.clone()).await.disposition,
        ToolDisposition::Succeeded
    );
    std::fs::rename(
        directory.join("references/note.txt"),
        directory.join("references/original.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink("original.txt", directory.join("references/note.txt")).unwrap();
    assert_eq!(
        run_in_process(&runtime, reference).await.disposition,
        ToolDisposition::Denied
    );
    assert_eq!(
        run_in_process(
            &runtime,
            ToolCall::Skill {
                name: "review".into(),
                resource: Some("references/original.txt".into())
            }
        )
        .await
        .disposition,
        ToolDisposition::Denied
    );
    std::fs::rename(&directory, temp.path().join("original-skill")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("SKILL.md"), &body).unwrap();
    assert_ne!(
        run_in_process(&runtime, call).await.disposition,
        ToolDisposition::Succeeded
    );
}

#[test]
fn undispatched_guard_failures_explain_safe_causes_and_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(temp.path(), config_value());
    for (error, cause, next) in [
        (
            super::ToolError::GuardRejected(super::guard::GuardStage::NamespaceBootstrap),
            "namespace bootstrap",
            "Enable unprivileged user namespaces",
        ),
        (
            super::ToolError::GuardRejected(super::guard::GuardStage::NativeManager),
            "native manager",
            "Restore the user service manager",
        ),
        (
            super::ToolError::Operation,
            "exact cause was not reported",
            "Check the Guard executable",
        ),
    ] {
        let intent = runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Command {
                program: "script".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
        );
        let observation = super::unstarted_observation(intent, error);
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert!(observation.attestation.is_none());
        assert!(observation.output.contains(cause));
        assert!(observation.output.contains(next));
        assert!(observation.output.contains("resend your message"));
        assert!(!observation.output.contains("/goal resume"));
        assert_eq!(
            observation.output.contains("development.log"),
            cfg!(debug_assertions)
        );
        assert!(observation.output.contains("no operation was dispatched"));
    }
}

fn ignored_discovery_fixture(roots: Value) -> (tempfile::TempDir, super::ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    std::fs::create_dir_all(workspace.join("notes/pruned")).unwrap();
    std::fs::write(
        workspace.join("notes/.gitignore"),
        b"*.cache\npruned/\n!.env\n!.env.*\n!.aws/\n!.ssh/\n!.gnupg/\n!.git/\n!.npmrc\n!.pypirc\n",
    )
    .unwrap();
    std::fs::write(workspace.join("notes/visible.txt"), b"hit visible\n").unwrap();
    std::fs::write(
        workspace.join("notes/ignored.cache"),
        b"hit IGNORED_PAYLOAD_CANARY\n",
    )
    .unwrap();
    std::fs::write(workspace.join("notes/pruned/.gitignore"), b"!leaf\n").unwrap();
    std::fs::File::create(workspace.join("notes/pruned/leaf"))
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    std::fs::File::create(workspace.join("notes/oversized.cache"))
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    for path in [
        ".ENV",
        ".Env.local",
        ".AWS/credentials",
        ".SSH/key",
        ".GNUPG/key",
        ".GIT/config",
        ".NPMRC",
        ".PYPIRC",
    ] {
        let path = workspace.join("notes").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"hit PROTECTED_PAYLOAD_CANARY\n").unwrap();
    }
    let runtime = in_process_runtime(
        workspace,
        json!({"version":1,"workspace_paths":roots,"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    (temp, runtime)
}

#[tokio::test]
async fn typed_discovery_prunes_ignored_payloads_and_preserves_explicit_access() {
    for roots in [json!(["."]), json!(["notes"])] {
        let (_temp, runtime) = ignored_discovery_fixture(roots);
        for call in [
            ToolCall::List {
                path: "notes".into(),
            },
            glob("**", "notes", 100),
            search("notes", "hit"),
            search("notes/pruned", "hit"),
        ] {
            let pruned_search =
                matches!(&call, ToolCall::Search { path, .. } if path == "notes/pruned");
            let result = run_in_process(&runtime, call).await;
            assert_eq!(result.disposition, ToolDisposition::Succeeded);
            if pruned_search {
                assert_eq!(
                    serde_json::from_str::<Value>(&result.output).unwrap()["entries"],
                    json!([])
                );
            } else {
                assert!(result.output.contains("visible.txt"));
            }
            for hidden in [
                "ignored.cache",
                "oversized.cache",
                "pruned/leaf",
                ".ENV",
                ".AWS",
                "PAYLOAD_CANARY",
            ] {
                assert!(!result.output.contains(hidden));
            }
        }
        for call in [
            ToolCall::Read {
                path: "notes/ignored.cache".into(),
                offset: None,
                limit: None,
            },
            search("notes/ignored.cache", "hit"),
        ] {
            let result = run_in_process(&runtime, call).await;
            assert_eq!(result.disposition, ToolDisposition::Succeeded);
            assert!(result.output.contains("IGNORED_PAYLOAD_CANARY"));
            assert!(matches!(
                result.attestation,
                Some(Attestation::InProcess { .. })
            ));
        }
    }
}

fn invalid_discovery_control(case: &str) -> (tempfile::TempDir, super::ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    std::fs::create_dir(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/visible.txt"), b"hit visible\n").unwrap();
    let control = workspace.join(".gitignore");
    match case {
        "encoding" => std::fs::write(&control, [0xff]).unwrap(),
        "bytes" => {
            std::fs::write(&control, vec![b'#'; super::selection::CONTROL_BYTES + 1]).unwrap()
        }
        "expansion" => std::fs::write(&control, "*".repeat(17)).unwrap(),
        "symlink" => std::os::unix::fs::symlink("notes/visible.txt", &control).unwrap(),
        "hardlink" => {
            std::fs::write(workspace.join("notes/control-source"), b"ignored").unwrap();
            std::fs::hard_link(workspace.join("notes/control-source"), &control).unwrap()
        }
        "permission" | "grant" => std::fs::write(&control, b"ignored").unwrap(),
        _ => unreachable!("closed corpus case"),
    }
    let roots = if case == "grant" {
        json!(["notes"])
    } else {
        json!(["."])
    };
    let mut runtime = in_process_runtime(
        workspace,
        json!({"version":1,"workspace_paths":roots,"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    if case == "permission" {
        let bytes = br#"{"permissions":{"deny":["Read(./.gitignore)"]}}"#;
        runtime.permissions = Some(super::PermissionSnapshot {
            config: std::sync::Arc::new(
                super::permissions::PermissionConfig::parse(bytes).unwrap(),
            ),
            digest: digest(bytes),
        });
    }
    (temp, runtime)
}

#[tokio::test]
async fn discovery_control_failures_discard_results_and_do_not_taint_exact_reads() {
    for case in [
        "encoding",
        "bytes",
        "expansion",
        "symlink",
        "hardlink",
        "permission",
        "grant",
    ] {
        let (_temp, runtime) = invalid_discovery_control(case);
        for call in [
            ToolCall::List {
                path: "notes".into(),
            },
            glob("**", "notes", 100),
            search("notes", "hit"),
        ] {
            let result = run_in_process(&runtime, call).await;
            assert_ne!(result.disposition, ToolDisposition::Succeeded, "{case}");
            assert!(!result.output.contains("visible.txt") && !result.output.contains("ignored"));
        }
        let result = run_in_process(&runtime, search("notes/visible.txt", "hit")).await;
        assert_eq!(result.disposition, ToolDisposition::Succeeded, "{case}");
    }
}

#[test]
fn undispatched_resource_limit_remains_a_stopping_limit() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(temp.path(), config_value());
    let intent = runtime.intent(
        crate::RunId::new(),
        crate::AgentRunId::new(),
        ToolCall::Command {
            program: "script".into(),
            args: Vec::new(),
            cwd: ".".into(),
        },
    );
    let mut observation = super::unstarted_observation(intent, super::ToolError::Limit);
    assert_eq!(observation.disposition, ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(super::LimitCause::Resource));
    assert!(observation.attestation.is_none());
    assert!(observation.valid());
    assert!(observation.stops_run());
    assert!(observation.output.contains("resource limit"));
    assert!(observation.output.contains("no operation was dispatched"));
    observation.limit_cause = Some(super::LimitCause::RequestedTime);
    assert!(observation.valid());
    assert!(!observation.stops_run());
}

#[test]
fn delete_and_move_preserve_bytes_and_refuse_stale_or_existing_targets() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let config: Config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap();
    let content = b"binary\0\xff\r\n";
    let source = temp.path().join("source");
    let destination = temp.path().join("nested/destination");
    std::fs::write(&source, content).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    let move_call = ToolCall::Move {
        source: "source".into(),
        destination: "nested/destination".into(),
        expected_digest: hex_digest(content),
    };
    let stale = ToolCall::Delete {
        path: "source".into(),
        expected_digest: hex_digest(b"stale"),
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
        expected_digest: hex_digest(b"replacement"),
    };
    assert!(matches!(
        super::fs::native(temp.path(), &occupied, &config),
        Err(super::ToolError::Conflict)
    ));
    assert_eq!(std::fs::read(&source).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    let delete = ToolCall::Delete {
        path: "nested/destination".into(),
        expected_digest: hex_digest(content),
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
    let config: Config = serde_json::from_value(json!({"version":1,"workspace_paths":["owned"],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap();
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
                expected_digest: hex_digest(b"source"),
            },
            ToolCall::Move {
                source: path.into(),
                destination: "owned/new".into(),
                expected_digest: hex_digest(b"source"),
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
            expected_digest: hex_digest(b"source"),
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
            expected_digest: hex_digest(b"source"),
        };
        assert!(!config.allows(&call), "{call:?}");
    }
    for path in ["../source", "/source", "owned/.env", "owned/.git/config"] {
        assert!(
            !ToolCall::Delete {
                path: path.into(),
                expected_digest: hex_digest(b"source")
            }
            .valid()
        );
    }
    let file = std::fs::File::create(temp.path().join("owned/large")).unwrap();
    file.set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
    for call in [
        ToolCall::Delete {
            path: "owned/large".into(),
            expected_digest: hex_digest(b""),
        },
        ToolCall::Move {
            source: "owned/large".into(),
            destination: "owned/new".into(),
            expected_digest: hex_digest(b""),
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

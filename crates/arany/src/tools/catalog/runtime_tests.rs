use super::catalog::{Family, Site, catalog_call_schemas, plan_schema};
use crate::session::WorkspaceIdentity;
use crate::tools::config::Config;
use crate::tools::skills::parse_json;
use crate::tools::tests::config_value;
use crate::tools::types::*;
use serde_json::{Value, json};

#[test]
fn tool_call_display_and_site_facts_cover_every_variant() {
    use crate::session::worker::ChangesetSource;
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
                changeset: CommandChangeset {
                    files: vec![FileChange {
                        path: "src".into(),
                        before: None,
                        after: Some(FileVersion::from_bytes(b"text", false)),
                    }],
                },
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
        ToolCall::GhPrList {
            repository: "github.com/o/r".into(),
            state: PrState::Open,
            base: None,
            head: None,
            limit: DEFAULT_PR_LIMIT,
        },
        ToolCall::GhPrView {
            repository: "github.com/o/r".into(),
            number: 1,
        },
        ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind {
                plan_digest: [3; 32],
                bytes: 1,
                sha256: [4; 32],
            },
        },
        ToolCall::Glob {
            pattern: "**/*.rs".into(),
            path: path(),
            limit: 1,
        },
    ];

    let expected = [
        ("Parse agent definition", "", vec![]),
        ("List", "src", vec!["src"]),
        ("Read", "src", vec!["src"]),
        ("Search", "q", vec!["src"]),
        ("Delete", "src", vec!["src"]),
        ("Move", "src", vec!["src", "destination"]),
        ("Create folder", "src", vec!["src"]),
        ("Write", "src", vec!["src"]),
        ("Edit", "src", vec!["src"]),
        ("MultiEdit", "src", vec!["src"]),
        ("Command", "cargo", vec!["."]),
        ("Unguarded command", "cargo", vec!["."]),
        ("Publish", "src", vec!["src"]),
        ("Skill", "guide", vec![]),
        ("MCP list", "local", vec![]),
        ("MCP", "local.t", vec![]),
        ("Git status", "src", vec!["src"]),
        ("Git diff", "src", vec!["src"]),
        ("Git log", "src", vec!["src"]),
        ("Git commit", "src", vec!["src"]),
        ("Git branches", "", vec![]),
        ("Git branch", "topic", vec![]),
        ("Git switch", "topic", vec![]),
        ("Git push", "topic", vec![]),
        ("PR create", "github.com/o/r", vec![]),
        ("PR list", "github.com/o/r", vec![]),
        ("PR view", "github.com/o/r", vec![]),
        ("Rewind publication", "", vec![]),
        ("Glob", "**/*.rs", vec!["src"]),
    ];
    assert_eq!(calls.len(), expected.len());
    let mut operations = std::collections::BTreeSet::new();
    for (call, (verb, subject, paths)) in calls.into_iter().zip(expected) {
        let display = call.display_facts();
        assert_eq!(
            (display.verb, display.subject.as_ref(), display.paths),
            (verb, subject, paths),
            "{call:?}"
        );
        assert_eq!(
            (display.family, display.site),
            (call.family(), call.site()),
            "{call:?}"
        );
        operations.insert(call.label());
        let old_is_host = matches!(
            call,
            ToolCall::GitPush { .. }
                | ToolCall::GhPrCreate { .. }
                | ToolCall::GhPrList { .. }
                | ToolCall::GhPrView { .. }
        );
        let old_is_unguarded = matches!(call, ToolCall::CommandUnguarded { .. });
        let old_is_internal = matches!(call, ToolCall::ParseAgentDefinition { .. });

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
    assert_eq!(operations.len(), super::catalog::ALL.len());
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
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        command_environment: std::sync::Arc::new(crate::tools::CommandEnvironment::for_path(
            "/usr/bin:/bin",
        )),
        spill_access: None,
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
        super::guard::git_branch_creation_available()
    );
    assert_eq!(
        config.allows(&ToolCall::GitCommit {
            message: "m".into(),
            paths: vec![".".into()],
            staged: false,
        }),
        super::guard::git_commit_available()
    );
    assert_eq!(
        config.allows(&ToolCall::GitSwitch {
            branch: "topic".into()
        }),
        super::guard::git_writes_available()
    );
    for git_writes in [false, true] {
        let creation = git_writes || super::guard::git_branch_creation_available();
        let operations = if git_writes {
            vec![
                "git_status",
                "git_diff",
                "git_log",
                "git_commit",
                "git_branch",
                "git_switch",
            ]
        } else if cfg!(target_os = "macos") {
            vec![
                "git_status",
                "git_diff",
                "git_log",
                "git_commit",
                "git_branch",
            ]
        } else {
            vec!["git_status", "git_diff", "git_log", "git_branch"]
        };
        let catalog = catalog(config.clone(), git_writes);
        assert_eq!(catalog["git"]["write"], creation);
        assert_eq!(catalog["git"]["operations"], json!(operations));
        let branch = catalog_call_schemas(&catalog)
            .into_iter()
            .find(|call| call["properties"]["operation"]["enum"][0] == "git_branch")
            .unwrap();
        let expected = if creation {
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
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        command_environment: std::sync::Arc::new(crate::tools::CommandEnvironment::for_path(
            "/usr/bin:/bin",
        )),
        spill_access: None,
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
        include_str!("../snapshots/catalog_prose_macos.json")
    } else {
        include_str!("../snapshots/catalog_prose.json")
    };
    assert!(rendered == expected, "{rendered}");
}

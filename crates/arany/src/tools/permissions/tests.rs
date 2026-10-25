use super::hooks::parse_output;
use super::*;
use crate::tools::{CommandChangeset, FileChange, FileVersion};
use serde_json::{Value, json};

fn config(permissions: Value) -> PermissionConfig {
    PermissionConfig::parse(&serde_json::to_vec(&json!({"permissions": permissions})).unwrap())
        .unwrap()
}

fn read(path: &str) -> ToolCall {
    ToolCall::Read {
        path: path.into(),
        offset: None,
        limit: Some(1),
    }
}

fn glob(path: &str) -> ToolCall {
    ToolCall::Glob {
        pattern: "**/*.rs".into(),
        path: path.into(),
        limit: 100,
    }
}

fn write(path: &str) -> ToolCall {
    ToolCall::Write {
        path: path.into(),
        expected_digest: None,
        content: "x".into(),
    }
}

fn multiedit(path: &str) -> ToolCall {
    ToolCall::MultiEdit {
        file_path: path.into(),
        expected_digest: "0".repeat(64),
        edits: vec![crate::LiteralEdit {
            old_string: "a".into(),
            new_string: "b".into(),
            replace_all: false,
        }],
    }
}

fn command(program: &str, args: &[&str]) -> ToolCall {
    ToolCall::Command {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        cwd: ".".into(),
    }
}

fn hooks(event: &str, matcher: Value, command: Value) -> Result<PermissionConfig, ConfigError> {
    PermissionConfig::parse(
        &serde_json::to_vec(&json!({"hooks": {event: [{"matcher": matcher, "hooks": [command]}]}}))
            .unwrap(),
    )
}

#[test]
fn permission_modes_map_existing_approval_and_narrow_plan_and_dont_ask() {
    let reads = read("src/lib.rs");
    let edits = write("src/lib.rs");
    let process = command("git", &["diff"]);
    // A Git write is a process mutation, not a typed edit: edit modes still ask for it.
    let git_write = ToolCall::GitCommit {
        message: "m".into(),
        paths: vec!["src/lib.rs".into()],
        staged: false,
    };
    use Decision::{Allow, Ask, Deny};
    for (mode, decisions) in [
        ("default", [Allow, Ask, Ask, Ask]),
        ("acceptEdits", [Allow, Allow, Ask, Ask]),
        ("auto", [Allow, Ask, Ask, Ask]),
        ("plan", [Allow, Deny, Deny, Deny]),
        ("dontAsk", [Allow, Deny, Deny, Deny]),
    ] {
        let parsed = config(json!({"defaultMode": mode}));
        assert_eq!(
            parsed.decision(&multiedit("src/lib.rs")),
            decisions[1],
            "{mode}"
        );
        for (call, expected) in [&reads, &edits, &process, &git_write]
            .into_iter()
            .zip(decisions)
        {
            assert_eq!(parsed.decision(call), expected, "{mode} {call:?}");
        }
    }
    assert!(ApprovalMode::AutoEdits.asks(&git_write) && !ApprovalMode::AutoEdits.asks(&edits));
    assert_eq!(
        config(json!({"allow": ["git_commit"]})).decision(&git_write),
        Allow
    );
    assert_eq!(
        config(json!({"defaultMode": "acceptEdits", "allow": ["git_commit"]})).decision(&git_write),
        Ask
    );
    assert_eq!(
        config(json!({"deny": ["Read(./src/**)"]})).decision(&git_write),
        Deny
    );
    for paths in [
        vec![],
        vec![".".to_owned()],
        vec!["docs".to_owned(), ".".to_owned()],
    ] {
        let whole = ToolCall::GitCommit {
            message: "m".into(),
            staged: paths.is_empty(),
            paths,
        };
        assert_eq!(
            config(json!({"deny": ["Read(./private/**)"]})).decision(&whole),
            Deny,
            "{whole:?}"
        );
    }
    let switch = ToolCall::GitSwitch {
        branch: "topic".into(),
    };
    assert_eq!(
        config(json!({"allow": ["git_switch"], "ask": ["Edit(./src/**)"]})).decision(&switch),
        Ask
    );
    let plan = config(json!({"defaultMode": "plan", "allow": ["Bash", "Edit"]}));
    assert_eq!(plan.decision(&process), Decision::Deny);
    assert_eq!(plan.decision(&edits), Decision::Deny);
    assert_eq!(
        plan.decision(&ToolCall::GitStatus { path: ".".into() }),
        Decision::Allow
    );
    assert_eq!(
        plan.decision(&ToolCall::McpList {
            server: "local".into()
        }),
        Decision::Deny
    );
    let dont_ask = config(json!({"defaultMode": "dontAsk", "allow": ["Bash(git diff *)"]}));
    assert_eq!(dont_ask.decision(&process), Decision::Allow);
    assert_eq!(config(json!({})).mode(), PermissionMode::Default);
}

#[test]
fn commit_paths_cannot_bypass_restrictive_descendant_rules() {
    let cases: &[(&str, &[&str], bool, bool)] = &[
        ("./src/secrets/**", &["src"], false, true),
        ("./src/secrets/key", &["src"], false, true),
        ("./SRC/secrets/**", &["src"], false, true),
        ("./docs/secrets/**", &["src"], false, false),
        ("./src-other/secrets/**", &["src"], false, false),
        ("./src/secrets/**", &["src/public"], false, false),
        ("**/secret", &["src"], false, true),
        ("./src/**/secret", &["src/public"], false, true),
        ("./docs/**/secret", &["src"], false, false),
        ("./src/s?cret/key", &["src"], false, true),
        ("./src/s[ae]cret/key", &["src"], false, true),
        ("./src/secrets/**", &["docs", "src"], false, true),
        ("./private/**", &["."], false, true),
        ("./private/**", &[], true, true),
        ("./private/**", &["s\u{00e9}cret"], false, true),
    ];
    for &(pattern, paths, staged, restricted) in cases {
        let call = ToolCall::GitCommit {
            message: "m".into(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            staged,
        };
        for tool in ["Read", "Edit"] {
            for (list, decision) in [("deny", Decision::Deny), ("ask", Decision::Ask)] {
                let rule = format!("{tool}({pattern})");
                let parsed = config(json!({"allow": ["git_commit"], (list): [rule]}));
                assert_eq!(
                    parsed.decision(&call),
                    if restricted {
                        decision
                    } else {
                        Decision::Allow
                    },
                    "{list} {rule}: {call:?}"
                );
            }
        }
    }
}

#[test]
fn permission_rule_precedence_is_deny_then_ask_then_allow() {
    for mode in ["default", "acceptEdits", "plan", "auto", "dontAsk"] {
        let mut value = json!({"defaultMode": mode, "allow": ["Read"], "ask": ["Read(./src/**)"], "deny": ["Read(./src/private/**)"]});
        let parsed = config(value.clone());
        assert_eq!(parsed.decision(&read("src/private/key")), Decision::Deny);
        assert_eq!(
            parsed.decision(&read("src/lib.rs")),
            if mode == "dontAsk" {
                Decision::Deny
            } else {
                Decision::Ask
            }
        );
        assert_eq!(parsed.decision(&read("docs/tools.md")), Decision::Allow);
        value["deny"] = json!(["Read(./src/**)", "Read"]);
        assert_eq!(config(value).decision(&read("src/lib.rs")), Decision::Deny);
    }
    let ask = config(json!({"defaultMode":"acceptEdits", "allow":["Edit"], "ask":["Edit"]}));
    assert_eq!(ask.decision(&write("a")), Decision::Ask);
}

#[test]
fn explicit_ask_uses_humans_and_decision_composition_only_narrows() {
    for mode in ["acceptEdits", "auto"] {
        let parsed = config(json!({"defaultMode":mode, "ask":["Edit"]}));
        assert_eq!(
            parsed.approval_mode(&write("a")),
            Some(ApprovalMode::Request)
        );
    }
    assert_eq!(
        config(json!({"defaultMode":"auto"})).approval_mode(&command("git", &["diff"])),
        Some(ApprovalMode::Auto)
    );
    assert_eq!(
        config(json!({"defaultMode":"dontAsk"})).approval_mode(&command("git", &["diff"])),
        None
    );
    for (base, restrictions) in [
        (
            Decision::Allow,
            [Decision::Allow, Decision::Ask, Decision::Deny],
        ),
        (
            Decision::Ask,
            [Decision::Ask, Decision::Ask, Decision::Deny],
        ),
        (
            Decision::Deny,
            [Decision::Deny, Decision::Deny, Decision::Deny],
        ),
    ] {
        for (restriction, expected) in [Decision::Allow, Decision::Ask, Decision::Deny]
            .into_iter()
            .zip(restrictions)
        {
            assert_eq!(base.narrow(restriction), expected);
        }
    }
}

#[test]
fn dont_ask_preserves_explicit_rule_provenance() {
    let call = ToolCall::CommandUnguarded {
        program: "printf".into(),
        args: vec!["host value".into()],
        cwd: String::new(),
        justification: "Inspect a synthetic host value".into(),
        prefix_rule: None,
    };
    for (rules, source) in [
        (json!({}), PermissionSource::Mode),
        (json!({"ask": ["Bash"]}), PermissionSource::Rule),
        (json!({"deny": ["Bash"]}), PermissionSource::Rule),
    ] {
        let mut permissions = rules;
        permissions["defaultMode"] = json!("dontAsk");
        assert_eq!(
            config(permissions).evaluate(&call),
            (Decision::Deny, source, None)
        );
    }
}

#[test]
fn bash_globs_follow_peer_word_boundaries_and_trailing_star() {
    for (pattern, program, args, matched) in [
        ("git diff *", "git", vec!["diff"], true),
        ("git diff *", "git", vec!["diff", "--stat", "main"], true),
        ("git diff *", "git", vec!["different"], false),
        ("git diff", "git", vec!["diff", "--stat"], false),
        ("git diff", "git", vec!["diff"], true),
        ("git:*", "git", vec![], true),
        ("git:*", "github", vec![], false),
        ("git:* push", "git", vec!["push"], false),
        ("git * main", "git", vec!["log", "main"], true),
        ("git * main", "git", vec!["main"], false),
        ("* --version", "node", vec!["--version"], true),
        ("* --help *", "npm", vec!["--help"], false),
        ("* --help *", "npm", vec!["--help", "x"], true),
        ("ls*", "lsof", vec![], true),
        ("ls *", "lsof", vec![], false),
        ("*", "anything", vec![], true),
    ] {
        let parsed = config(json!({"allow": [format!("Bash({pattern})")]}));
        assert_eq!(
            parsed.decision(&command(program, &args)),
            if matched {
                Decision::Allow
            } else {
                Decision::Ask
            },
            "{pattern} {program} {args:?}"
        );
    }
}

#[test]
fn opaque_argv_never_auto_allows_and_conservatively_matches_restrictions() {
    for args in [
        vec!["-c", "git diff; rm x"],
        vec!["a b"],
        vec![""],
        vec!["$(git diff)"],
        vec!["'diff'"],
        vec!["diff\nrm x"],
    ] {
        let call = command("sh", &args);
        for (list, expected) in [
            ("allow", Decision::Ask),
            ("ask", Decision::Ask),
            ("deny", Decision::Deny),
        ] {
            assert_eq!(
                config(json!({list:["Bash(git diff *)"]})).decision(&call),
                expected,
                "{list} {args:?}"
            );
        }
    }
    let oversized = command("git", &vec!["diff"; 65]);
    assert_eq!(
        config(json!({"allow":["Bash"]})).decision(&oversized),
        Decision::Deny
    );
    let args = (0..5).map(|_| "x".repeat(4096)).collect();
    let large = ToolCall::Command {
        program: "git".into(),
        args,
        cwd: ".".into(),
    };
    assert_eq!(
        config(json!({"allow":["Bash(*)"]})).decision(&large),
        Decision::Deny
    );
    assert_eq!(
        config(json!({"deny":["Bash(diff)"]})).decision(&large),
        Decision::Deny
    );
}

#[test]
fn file_globs_follow_segment_depth_anchors_and_peer_directory_rules() {
    for (pattern, path, matched) in [
        ("./src/**", "src/lib.rs", true),
        ("./src/**", "vendor/src/lib.rs", false),
        ("/src/**", "src/lib.rs", true),
        ("/src/**", "vendor/src/lib.rs", false),
        ("src/**/*.rs", "src/lib.rs", true),
        ("src/**/*.rs", "src/deep/lib.rs", true),
        ("src/*.rs", "src/deep/lib.rs", false),
        ("src/*.rs", "src/lib.rs", true),
        ("*.rs", "vendor/lib.rs", true),
        ("./*.rs", "vendor/lib.rs", false),
        ("src/?.rs", "src/a.rs", true),
        ("src/?.rs", "src/é.rs", true),
        ("src/?.rs", "src/ab.rs", false),
        ("src/[a-c].rs", "src/b.rs", true),
        ("src/[!a-c].rs", "src/d.rs", true),
        ("src/[!a-c].rs", "src/b.rs", false),
        ("src[!x]a.rs", "src/a.rs", false),
        ("src/file\\[1\\].rs", "src/file[1].rs", true),
        ("./Finance (2024)/**", "Finance (2024)/report", true),
        ("src/**/nested/*.rs", "src/nested/a.rs", true),
        ("src/**/nested/*.rs", "src/a/nested/b.rs", true),
        ("src/components/**", "vendor/src/components/x", false),
        ("**/src/**", "vendor/src/lib.rs", true),
    ] {
        let parsed = config(json!({"deny": [format!("Read({pattern})")]}));
        assert_eq!(
            parsed.decision(&read(path)),
            if matched {
                Decision::Deny
            } else {
                Decision::Allow
            },
            "{pattern} {path}"
        );
    }
    for (list, expected) in [
        ("deny", Decision::Deny),
        ("ask", Decision::Ask),
        ("allow", Decision::Ask),
    ] {
        assert_eq!(
            config(json!({list:["Edit(src/**)"]})).decision(&write("vendor/src/a")),
            expected
        );
    }
    let root = ToolCall::List { path: "src".into() };
    assert_eq!(
        config(json!({"deny":["Read(src/**)"]})).decision(&root),
        Decision::Deny
    );
}

#[test]
fn tool_mappings_cover_typed_operations_and_read_denials_cover_writes() {
    let parsed = config(json!({"deny":["Read(./private/**)"]}));
    for call in [
        read("private/a"),
        write("private/a"),
        multiedit("private/a"),
        ToolCall::Mkdir {
            path: "private/a".into(),
        },
        ToolCall::Search {
            path: "private/a".into(),
            query: "x".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::List {
            path: "private/a".into(),
        },
        glob("private/a"),
    ] {
        assert_eq!(parsed.decision(&call), Decision::Deny, "{call:?}");
    }
    for (rule, call) in [
        ("Edit(./src/**)", write("src/a")),
        ("Edit(./src/**)", multiedit("src/a")),
        ("MultiEdit(./src/**)", multiedit("src/a")),
        ("multiedit(./src/**)", multiedit("src/a")),
        ("Write", write("src/a")),
        ("read(./src/**)", read("src/a")),
        (
            "Grep(./src/**)",
            ToolCall::Search {
                path: "src/a".into(),
                query: "x".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
        ),
        ("Glob", ToolCall::List { path: "src".into() }),
        ("Glob", glob("src")),
        ("glob(./src/**)", glob("src/a")),
        ("Read(./src/**)", glob("src/a")),
        ("Read", glob(".")),
        (
            "Skill(guide*)",
            ToolCall::Skill {
                name: "guide".into(),
                resource: None,
            },
        ),
        ("git_*", ToolCall::GitStatus { path: ".".into() }),
        (
            "mcp_*",
            ToolCall::McpList {
                server: "local".into(),
            },
        ),
        ("B*", command("git", &["diff"])),
        ("*", read("a")),
    ] {
        assert_eq!(
            config(json!({"deny":[rule]})).decision(&call),
            Decision::Deny,
            "{rule}"
        );
    }
    // The native name selects only its own operation; the peer Glob also keeps List.
    assert_eq!(
        config(json!({"deny":["glob"]})).decision(&ToolCall::List { path: "src".into() }),
        Decision::Allow
    );
    assert_eq!(
        config(json!({"ask":["glob"]})).decision(&glob("src")),
        Decision::Ask
    );
    assert_eq!(
        config(json!({"allow":["Edit(./src/**)"]})).decision(&multiedit("src/a")),
        Decision::Allow
    );
    assert_eq!(
        config(json!({"ask":["Edit(./src/**)"]})).decision(&multiedit("src/a")),
        Decision::Ask
    );
    assert_eq!(
        config(json!({"deny":["Read(./src/**)"],"allow":["Edit"]})).decision(&multiedit("src/a")),
        Decision::Deny
    );
    assert_eq!(
        config(json!({"deny":["Read(./private/**)"]})).decision(&glob("src")),
        Decision::Allow
    );
    assert!(PermissionConfig::parse(br#"{"permissions":{"deny":["Glob(./src/**)"]}}"#).is_err());
    assert_eq!(
        config(json!({"ask":["Read"]})).decision(&write("src/a")),
        Decision::Ask
    );
    assert_eq!(
        config(json!({"defaultMode":"acceptEdits", "ask":["Read"]})).decision(&write("src/a")),
        Decision::Allow
    );
}

#[test]
fn changeset_rules_require_all_paths_for_allow_and_any_path_for_deny_or_ask() {
    let batch = |paths: &[&str]| ToolCall::ApplyChangeset {
        source: crate::session::worker::ChangesetSource::CommandReceipt {
            source_intent_digest: [1; 32],
            changeset: CommandChangeset {
                files: paths
                    .iter()
                    .map(|path| FileChange {
                        path: (*path).into(),
                        before: None,
                        after: Some(FileVersion::from_bytes(b"x", false)),
                    })
                    .collect(),
            },
        },
    };
    let mixed = batch(&["docs/a", "src/a"]);
    assert_eq!(
        config(json!({"allow":["Edit(src/**)"]})).decision(&mixed),
        Decision::Ask
    );
    assert_eq!(
        config(json!({"deny":["Edit(src/**)"]})).decision(&mixed),
        Decision::Deny
    );
    assert_eq!(
        config(json!({"defaultMode":"acceptEdits", "ask":["Edit(src/**)"]})).decision(&mixed),
        Decision::Ask
    );
    assert_eq!(
        config(json!({"allow":["Edit(src/**)"]})).decision(&batch(&["src/a", "src/b"])),
        Decision::Allow
    );
    assert_eq!(
        config(json!({"deny":["Read(src/**)"]})).decision(&mixed),
        Decision::Deny
    );

    use crate::session::worker::*;
    let assignment_id = crate::session::AgentRunId::new();
    let worker = ToolCall::ApplyChangeset {
        source: ChangesetSource::WorkerHandover {
            assignment_id,
            handover_sequence: 3,
            data: WorkerDataRef {
                assignment_id,
                purpose: WorkerDataPurpose::Handover,
                first_sequence: 1,
                seal_sequence: 2,
                chunks: 1,
                bytes: 1,
                sha256: [1; 32],
            },
        },
    };
    for (settings, paths) in [
        (json!({"allow":["Edit(src/**)"]}), vec!["docs/a", "src/a"]),
        (json!({"deny":["Edit(src/**)"]}), vec!["docs/a", "src/a"]),
        (
            json!({"defaultMode":"acceptEdits","ask":["Edit(src/**)"]}),
            vec!["docs/a", "src/a"],
        ),
        (json!({"allow":["Edit(src/**)"]}), vec!["src/a", "src/b"]),
        (json!({"deny":["Read(src/**)"]}), vec!["docs/a", "src/a"]),
        (
            json!({"defaultMode":"acceptEdits","deny":["Edit(other/**)"]}),
            vec!["src/a"],
        ),
    ] {
        let files: Vec<_> = paths
            .iter()
            .map(|path| WorkerFileFact {
                path: (*path).into(),
                before: WorkerBaseVersion::Absent,
                after: WorkerBaseVersion::File {
                    sha256: crate::validate::hex_digest(b"x"),
                    bytes: 1,
                    executable: false,
                },
            })
            .collect();
        let permissions = config(settings);
        assert_eq!(
            permissions.evaluate_worker_paths(&worker, Some(&files)),
            permissions.evaluate(&batch(&paths))
        );
    }
}

#[test]
fn malformed_and_unsupported_permission_rules_fail_closed_without_echoing_input() {
    for rule in [
        "",
        "Unknown",
        "B* )",
        "B* name",
        "(*)",
        "Read(",
        "Read()",
        " Read",
        "Read\n",
        "Bash(x) trailing",
        "Write(src/**)",
        "Glob(src/**)",
        "Read(../x)",
        "Read(a//b)",
        "Read(~/x)",
        "Read(//tmp/x)",
        "Read(!x)",
        "Read(#x)",
        "Read(x/**a)",
        "Read(x/a**)",
        "Read(x/[)",
        "Read(x/[z-a])",
        "Read(x/\\)",
        "mcp_call(x)",
    ] {
        let bytes = serde_json::to_vec(&json!({"permissions":{"deny":[rule]}})).unwrap();
        let error = PermissionConfig::parse(&bytes).unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Rule {
                    list: "deny",
                    index: 0,
                    ..
                }
            ),
            "{rule:?}: {error}"
        );
    }
    assert!(PermissionConfig::parse(br#"{"permissions":{"allow":["B*"]}}"#).is_err());
    for bytes in [
        br#"{"permissions":{"defaultMode":"bypassPermissions"}}"#.as_slice(),
        br#"{"permissions":{"defaultMode":"manual"}}"#,
        br#"{"permissions":{"allow":["Read"],"allow":[]}}"#,
        br#"{"permissions":{},"permissions":{}}"#,
        br#"{"permissions":{"allow":"Read"}}"#,
        br#"{"permissions":{"unknown":true}}"#,
        br#"{"other":true}"#,
        br#"{} {}"#,
        b"\xff",
    ] {
        assert!(PermissionConfig::parse(bytes).is_err());
    }
}

#[test]
fn permission_limits_accept_boundaries_and_reject_overflow() {
    assert!(
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions":{"allow":vec!["Read"; MAX_RULES]}})).unwrap()
        )
        .is_ok()
    );
    assert!(matches!(
        PermissionConfig::parse(
            &serde_json::to_vec(
                &json!({"permissions":{"allow":vec!["Read"; MAX_RULES], "deny":["Read"]}})
            )
            .unwrap()
        ),
        Err(ConfigError::Limit(_))
    ));
    let exact = format!("Read({})", "a".repeat(MAX_RULE_PATTERN_BYTES - 6));
    assert!(
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions":{"deny":[exact]}})).unwrap()
        )
        .is_ok()
    );
    let over = format!("Read({})", "a".repeat(MAX_RULE_PATTERN_BYTES - 5));
    assert!(
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions":{"deny":[over]}})).unwrap()
        )
        .is_err()
    );
    let mut exact = b"{}".to_vec();
    exact.resize(MAX_CONFIG_BYTES, b' ');
    assert!(PermissionConfig::parse(&exact).is_ok());
    exact.push(b' ');
    assert!(matches!(
        PermissionConfig::parse(&exact),
        Err(ConfigError::Json)
    ));
    let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
    assert!(PermissionConfig::parse(deep.as_bytes()).is_err());
    for call in [
        read("../x"),
        read("/tmp/x"),
        read(".env"),
        command("INVALID", &[]),
    ] {
        assert_eq!(
            config(json!({"allow":["Read","Bash"]})).decision(&call),
            Decision::Deny
        );
    }
}

#[test]
fn hook_schema_accepts_peer_events_defaults_exec_form_and_inert_commands() {
    let command = "do-not-execute $(exit 9)";
    let settings = json!({"hooks":{
        "PreToolUse":[{"matcher":"Bash", "hooks":[{"type":"command", "command":command}]}],
        "PostToolUse":[{"matcher":"Edit|Write", "hooks":[{"type":"command", "command":"check", "args":[], "timeout":1}]}],
        "Stop":[{"hooks":[{"type":"command", "command":"check", "timeout":600}]}],
        "SubagentStop":[{"matcher":"Explore", "hooks":[{"type":"command", "command":"check"}]}],
        "PreCompact":[{"matcher":"manual|auto", "hooks":[{"type":"command", "command":"check"}]}]
    }});
    let parsed = PermissionConfig::parse(&serde_json::to_vec(&settings).unwrap()).unwrap();
    let hook_config = parsed.hooks();
    let pre = &hook_config.groups(HookEvent::PreToolUse)[0];
    assert!(pre.matches_tool(&command_call()).unwrap());
    assert!(!pre.matches_tool(&read("a")).unwrap());
    assert_eq!(pre.commands()[0].command(), command);
    assert_eq!(pre.commands()[0].args(), None);
    assert_eq!(pre.commands()[0].timeout_seconds(), 600);
    let post = &hook_config.groups(HookEvent::PostToolUse)[0];
    assert!(post.matches_tool(&write("a")).unwrap());
    assert_eq!(post.commands()[0].args(), Some([].as_slice()));
    assert_eq!(post.commands()[0].timeout_seconds(), 1);
    assert!(hook_config.groups(HookEvent::Stop)[0].matches("").unwrap());
    assert!(
        hook_config.groups(HookEvent::SubagentStop)[0]
            .matches("Explore")
            .unwrap()
    );
    assert!(
        hook_config.groups(HookEvent::PreCompact)[0]
            .matches("auto")
            .unwrap()
    );
    assert!(config(json!({})).hooks().groups(HookEvent::Stop).is_empty());
}

fn command_call() -> ToolCall {
    command("git", &["diff"])
}

#[test]
fn hook_matchers_use_exact_alternatives_or_bounded_unanchored_regex() {
    for (matcher, yes, no) in [
        ("Bash", "Bash", "BashExtra"),
        ("Edit|Write", "Write", "NotebookEdit"),
        ("Edit, Write", "Edit", "NotebookEdit"),
        ("Edit.*", "NotebookEdit", "Read"),
        ("^Edit$", "Edit", "NotebookEdit"),
        ("MultiEdit", "MultiEdit", "Edit"),
    ] {
        let parsed = hooks(
            "PreToolUse",
            json!(matcher),
            json!({"type":"command","command":"x"}),
        )
        .unwrap();
        let group = &parsed.hooks().groups(HookEvent::PreToolUse)[0];
        assert!(group.matches(yes).unwrap(), "{matcher}");
        assert!(!group.matches(no).unwrap(), "{matcher}");
        assert!(
            group
                .matches(&"x".repeat(MAX_RULE_PATTERN_BYTES + 1))
                .is_err()
        );
        for subject in ["Bash\n", "Bash\0", "Bash\u{1b}"] {
            assert!(group.matches(subject).is_err());
        }
        assert!(group.matches_tool(&read("bad\npath")).is_err());
    }
    let peer = hooks(
        "PreToolUse",
        json!("Glob"),
        json!({"type":"command","command":"x"}),
    )
    .unwrap();
    let group = &peer.hooks().groups(HookEvent::PreToolUse)[0];
    assert!(group.matches_tool(&glob(".")).unwrap());
    assert!(
        group
            .matches_tool(&ToolCall::List { path: ".".into() })
            .unwrap()
    );
    assert!(!group.matches_tool(&read("a")).unwrap());
    let peer = hooks(
        "PreToolUse",
        json!("MultiEdit"),
        json!({"type":"command","command":"x"}),
    )
    .unwrap();
    assert!(
        peer.hooks().groups(HookEvent::PreToolUse)[0]
            .matches_tool(&multiedit("a"))
            .unwrap()
    );
    for matcher in ["", "*"] {
        let parsed = hooks(
            "PreCompact",
            json!(matcher),
            json!({"type":"command","command":"x"}),
        )
        .unwrap();
        assert!(
            parsed.hooks().groups(HookEvent::PreCompact)[0]
                .matches("auto")
                .unwrap()
        );
    }
}

#[test]
fn hook_schema_rejects_ambiguous_unsupported_or_unsafe_config() {
    for value in [
        json!({"type":"prompt","command":"x"}),
        json!({"type":"command","command":""}),
        json!({"type":"command","command":" \t"}),
        json!({"type":"command","command":"x\0"}),
        json!({"type":"command","command":"x", "timeout":0}),
        json!({"type":"command","command":"x", "timeout":601}),
        json!({"type":"command","command":"x", "timeout":1.5}),
        json!({"type":"command","command":"x", "timeout":-1}),
        json!({"type":"command","command":"x", "async":true}),
        json!({"type":"command","command":"x", "args":["\0"]}),
        json!({"type":"command", "command":"x", "args":null}),
        json!({"command":"x"}),
        json!({"type":"command"}),
    ] {
        assert!(hooks("PreToolUse", json!("Bash"), value).is_err());
    }
    for matcher in ["[", "(?=Bash)", "(Bash)\\1", "Bash|", "Bash\n", " "] {
        assert!(
            hooks(
                "PreToolUse",
                json!(matcher),
                json!({"type":"command","command":"x"})
            )
            .is_err(),
            "{matcher:?}"
        );
    }
    assert!(
        hooks(
            "Stop",
            json!("Bash"),
            json!({"type":"command","command":"x"})
        )
        .is_err()
    );
    assert!(
        hooks(
            "Unknown",
            json!("*"),
            json!({"type":"command","command":"x"})
        )
        .is_err()
    );
    for bytes in [
        br#"{"hooks":{"Stop":[],"Stop":[]}}"#.as_slice(),
        br#"{"hooks":{"Stop":[{"hooks":[]}]}}"#,
        br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"a","command":"b"}]}]}}"#,
        br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"a"}],"unknown":true}]}}"#,
    ] {
        assert!(PermissionConfig::parse(bytes).is_err());
    }
}

#[test]
fn hook_limits_accept_boundaries_and_reject_aggregate_overflow() {
    let handler = json!({"type":"command", "command":"x"});
    let group = json!({"hooks":[handler.clone(), handler.clone()]});
    let exact = json!({"hooks":{"PreToolUse":vec![group.clone();64]}});
    assert!(PermissionConfig::parse(&serde_json::to_vec(&exact).unwrap()).is_ok());
    let over_groups =
        json!({"hooks":{"PreToolUse":vec![group;64], "Stop":[{"hooks":[handler.clone()]}]}});
    assert!(matches!(
        PermissionConfig::parse(&serde_json::to_vec(&over_groups).unwrap()),
        Err(ConfigError::Limit(_))
    ));
    let over_hooks = json!({"hooks":{"PreToolUse":[{"hooks":vec![handler;128]}], "Stop":[{"hooks":[{"type":"command","command":"x"}]}]}});
    assert!(matches!(
        PermissionConfig::parse(&serde_json::to_vec(&over_hooks).unwrap()),
        Err(ConfigError::Limit(_))
    ));
    for (size, accepted) in [(4096, true), (4097, false)] {
        assert_eq!(
            hooks(
                "PreToolUse",
                json!("*"),
                json!({"type":"command","command":"x".repeat(size)})
            )
            .is_ok(),
            accepted
        );
    }
    for (size, accepted) in [(1024, true), (1025, false)] {
        assert_eq!(
            hooks(
                "PreToolUse",
                json!("x".repeat(size)),
                json!({"type":"command","command":"x"})
            )
            .is_ok(),
            accepted
        );
    }
    for (count, accepted) in [(64, true), (65, false)] {
        assert_eq!(
            hooks(
                "PreToolUse",
                json!("*"),
                json!({"type":"command","command":"x", "args":vec!["a"; count]})
            )
            .is_ok(),
            accepted
        );
    }
    assert!(
        hooks(
            "PreToolUse",
            json!("a{1000000}"),
            json!({"type":"command","command":"x"})
        )
        .is_err()
    );
}

#[test]
fn hostile_rule_text_is_total_and_deterministic() {
    for symbol in ['(', ')', '*', '[', ']', '\\', '/', '!', 'é', '\0', '\n'] {
        for count in [0, 1, 2, 32, 1024, 1025] {
            let rule = format!("Read({})", symbol.to_string().repeat(count));
            let bytes = serde_json::to_vec(&json!({"permissions":{"deny":[rule]}})).unwrap();
            match PermissionConfig::parse(&bytes) {
                Ok(parsed) => assert_eq!(
                    parsed.decision(&read("src/a")),
                    parsed.decision(&read("src/a"))
                ),
                Err(error) => assert!(!error.to_string().contains(['\0', '\n'])),
            }
        }
    }
}

fn mcp_call(server: &str, tool: &str) -> ToolCall {
    ToolCall::McpCall {
        server: server.into(),
        tool: tool.into(),
        schema_digest: "0".repeat(64),
        arguments: "{}".into(),
    }
}

#[test]
fn auto_allow_rules_never_bypass_execution_review() {
    for (rule, call) in [
        ("Bash", command("python", &["script.py"])),
        ("Bash(*)", command("python", &["script.py"])),
        ("Bash(python*)", command("python", &["script.py"])),
        ("Bash(python *)", command("python", &["script.py"])),
        ("Bash(python:*)", command("python", &["script.py"])),
        ("Bash(git diff *)", command("git", &["diff"])),
        ("Bash(git diff)", command("git", &["diff"])),
        ("Bash(npm run *)", command("npm", &["run", "build"])),
        ("command", command("python", &["script.py"])),
        ("mcp_call", mcp_call("memory", "read")),
        ("mcp__memory", mcp_call("memory", "read")),
        ("mcp__memory__*", mcp_call("memory", "read")),
        ("mcp__memory__read", mcp_call("memory", "read")),
        (
            "mcp_list",
            ToolCall::McpList {
                server: "memory".into(),
            },
        ),
        ("git_status", ToolCall::GitStatus { path: ".".into() }),
        ("Edit", write("src/a")),
    ] {
        let parsed = config(json!({"defaultMode":"auto", "allow":[rule]}));
        assert_eq!(parsed.decision(&call), Decision::Ask, "{rule}");
        assert_eq!(
            parsed.approval_mode(&call),
            Some(ApprovalMode::Auto),
            "{rule}"
        );
        let human = config(json!({"defaultMode":"auto", "allow":[rule], "ask":[rule]}));
        assert_eq!(
            human.approval_mode(&call),
            Some(ApprovalMode::Request),
            "{rule}"
        );
        let denied = config(json!({"defaultMode":"auto", "allow":[rule], "deny":[rule]}));
        assert_eq!(denied.decision(&call), Decision::Deny, "{rule}");
    }
}

#[test]
fn default_allows_are_explicit_preapprovals_and_accept_edits_keeps_command_approval() {
    for (rule, call) in [
        ("Bash", command("sh", &["-c", "echo x"])),
        ("Bash(*)", command("sh", &["-c", "echo x"])),
        ("Bash(git diff *)", command("git", &["diff"])),
        ("mcp_call", mcp_call("memory", "read")),
    ] {
        assert_eq!(
            config(json!({})).approval_mode(&call),
            Some(ApprovalMode::Request)
        );
        let parsed = config(json!({"defaultMode":"default", "allow":[rule]}));
        assert_eq!(parsed.decision(&call), Decision::Allow, "{rule}");
        assert_eq!(parsed.approval_mode(&call), None, "{rule}");
        let edits = config(json!({"defaultMode":"acceptEdits", "allow":[rule]}));
        assert_eq!(edits.decision(&call), Decision::Ask, "{rule}");
        assert_eq!(
            edits.approval_mode(&call),
            Some(ApprovalMode::Request),
            "{rule}"
        );
    }
}

#[test]
fn restrictive_paths_cover_case_and_normalization_aliases_without_widening_allows() {
    for (list, expected) in [("deny", Decision::Deny), ("ask", Decision::Ask)] {
        let parsed = config(json!({list:["Read(./secrets/**)", "Edit(./secrets/**)"]}));
        for call in [
            read("Secrets/key"),
            read("SECRETS/key"),
            read("secrets/café"),
            read("secrets/cafe\u{301}"),
            read("sécrets/key"),
            read("se\u{301}crets/key"),
            write("Secrets/key"),
        ] {
            assert_eq!(parsed.decision(&call), expected, "{list} {call:?}");
        }
        for pattern in [
            "Read(./sécrets/**)",
            "Read(./se\u{301}crets/**)",
            "Edit(./sécrets/**)",
            "git_diff(./sécrets/**)",
        ] {
            assert!(
                PermissionConfig::parse(
                    &serde_json::to_vec(&json!({"permissions":{list:[pattern]}})).unwrap()
                )
                .is_err(),
                "{list} {pattern}"
            );
        }
    }
    let allow = config(json!({"allow":["Edit(./secrets/**)"]}));
    assert_eq!(allow.decision(&write("secrets/a")), Decision::Allow);
    assert_eq!(allow.decision(&write("Secrets/a")), Decision::Ask);
    let unicode = config(json!({"allow":["Edit(./sécrets/**)"]}));
    assert_eq!(unicode.decision(&write("sécrets/a")), Decision::Allow);
    assert_eq!(unicode.decision(&write("se\u{301}crets/a")), Decision::Ask);
}

#[test]
fn peer_mcp_names_match_real_servers_tools_and_hook_subjects() {
    for (rule, server, tool, matched) in [
        ("mcp__*", "memory", "read", true),
        ("mcp__memory", "memory", "read", true),
        ("mcp__memory__*", "memory", "read", true),
        ("mcp__memory__read", "memory", "read", true),
        ("mcp__memory__read*", "memory", "read_file", true),
        ("mcp__memory__read", "other", "read", false),
        ("mcp__memory__read", "memory", "write", false),
        ("mcp__memory__read", "memory", "read_extra", false),
        ("mcp__a__b__c", "a__b", "c", true),
    ] {
        let parsed = config(json!({"deny":[rule]}));
        assert_eq!(
            parsed.decision(&mcp_call(server, tool)),
            if matched {
                Decision::Deny
            } else {
                Decision::Ask
            },
            "{rule} {server} {tool}"
        );
    }
    for rule in ["mcp__*", "mcp__memory"] {
        assert_eq!(
            config(json!({"deny":[rule]})).decision(&ToolCall::McpList {
                server: "memory".into()
            }),
            Decision::Deny
        );
    }
    assert_eq!(
        config(json!({"allow":["mcp__memory__read*"]})).decision(&mcp_call("memory", "read_file")),
        Decision::Allow
    );
    assert_eq!(
        config(json!({"ask":["mcp__memory__read*"]}))
            .approval_mode(&mcp_call("memory", "read_file")),
        Some(ApprovalMode::Request)
    );
    for rule in ["Zzz*", "mcp__", "mcp__memory__", "mcp__memory__read(x)"] {
        assert!(
            PermissionConfig::parse(
                &serde_json::to_vec(&json!({"permissions":{"deny":[rule]}})).unwrap()
            )
            .is_err(),
            "{rule}"
        );
    }
    assert!(PermissionConfig::parse(br#"{"permissions":{"allow":["mcp__*"]}}"#).is_err());
    for (rule, call) in [
        ("mcp__a__b", mcp_call("a__b", "read")),
        (
            "mcp__a__b",
            ToolCall::McpList {
                server: "a__b".into(),
            },
        ),
        ("mcp__a___b", mcp_call("a_", "b")),
        ("mcp__a__b__c", mcp_call("a__b", "c")),
    ] {
        for (list, expected) in [("deny", Decision::Deny), ("ask", Decision::Ask)] {
            let parsed = config(json!({list:[rule], "allow":["mcp_call", "mcp_list"]}));
            assert_eq!(parsed.decision(&call), expected, "{list} {rule} {call:?}");
        }
    }
    for (rule, server, tool) in [
        ("mcp__a__b", "a__b", "read"),
        ("mcp__a___b", "a_", "b"),
        ("mcp__a__b__c", "a__b", "c"),
    ] {
        assert_eq!(
            config(json!({"allow":[rule]})).decision(&mcp_call(server, tool)),
            Decision::Ask,
            "allow {rule} {server} {tool}"
        );
    }
    assert_eq!(
        config(json!({"allow":["mcp__a__b"]})).decision(&mcp_call("a", "b")),
        Decision::Allow
    );

    for matcher in [
        "mcp__memory__read",
        "mcp__memory__.*",
        "^mcp__memory__read$",
    ] {
        let parsed = hooks(
            "PreToolUse",
            json!(matcher),
            json!({"type":"command","command":"x"}),
        )
        .unwrap();
        let group = &parsed.hooks().groups(HookEvent::PreToolUse)[0];
        assert!(
            group.matches_tool(&mcp_call("memory", "read")).unwrap(),
            "{matcher}"
        );
        assert!(
            !group.matches_tool(&mcp_call("other", "read")).unwrap(),
            "{matcher}"
        );
        assert!(
            !group.matches_tool(&read("memory/read")).unwrap(),
            "{matcher}"
        );
        assert!(
            group
                .matches_tool(&mcp_call("ambiguous__server", "read"))
                .is_err()
        );
    }
}

#[test]
fn read_restrictions_cover_git_paths_without_preapproving_git() {
    for (list, expected) in [("deny", Decision::Deny), ("ask", Decision::Ask)] {
        let parsed = config(json!({list:["Read(./private/**)"]}));
        for call in [
            ToolCall::GitStatus {
                path: "private/key".into(),
            },
            ToolCall::GitDiff {
                path: "private/key".into(),
                base: None,
                head: None,
                staged: false,
            },
            ToolCall::GitLog {
                path: "Private/key".into(),
                revision: None,
                max_count: 1,
            },
        ] {
            assert_eq!(parsed.decision(&call), expected, "{list} {call:?}");
            assert_eq!(
                parsed.approval_mode(&call),
                if expected == Decision::Ask {
                    Some(ApprovalMode::Request)
                } else {
                    None
                }
            );
        }
    }
    assert_eq!(
        config(json!({"allow":["Read(./private/**)"]})).decision(&ToolCall::GitStatus {
            path: "private/key".into()
        }),
        Decision::Allow
    );
}

#[test]
fn restrictions_hide_result_paths_of_broader_calls_they_did_not_resolve() {
    let parsed = config(json!({
        "deny": ["Read(./private/**)", "Grep(./logs/**)"],
        "ask": ["Read(**/*.key)"],
        "allow": ["Read(./open/**)"],
    }));
    let search = |path: &str| ToolCall::Search {
        path: path.into(),
        query: "x".into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset: 0,
        limit: 100,
    };
    let list = ToolCall::List { path: ".".into() };
    let status = ToolCall::GitStatus { path: ".".into() };
    for (call, path, hidden) in [
        (&list, "private", true),
        (&list, "Private/deep/x", true),
        (&list, "src/a.KEY", true),
        (&list, "src/caf\u{e9}.rs", true),
        (&list, "open/a.rs", false),
        (&list, "logs/a", false),
        (&search("."), "logs/a", true),
        (&search("."), "src/a.rs", false),
        (&glob(""), "private/a.rs", true),
        (&status, "private/a", true),
        (&branch_call(None), "private/a", true),
        (&branch_call(None), "src/a.key", true),
        // A matching ask on the call's own path was approved before dispatch.
        (&search("a.key"), "a.key", false),
        (&search("vault"), "vault/b.key", true),
    ] {
        assert_eq!(
            parsed.hidden_paths(call).covers(path),
            hidden,
            "{call:?} {path}"
        );
    }
    assert!(
        config(json!({"allow": ["Read(./open/**)"]}))
            .hidden_paths(&list)
            .is_empty()
    );
    assert!(
        config(json!({"deny": ["Read"]}))
            .hidden_paths(&list)
            .is_empty()
    );
}

#[test]
fn bash_restrictions_cover_word_boundary_suffixes_without_expanding_allows() {
    for (program, args) in [
        ("timeout", vec!["30", "rm", "x"]),
        ("nice", vec!["rm", "x"]),
        ("xargs", vec!["rm", "x"]),
        ("env", vec!["FOO=1", "rm", "x"]),
        ("timeout", vec!["30", "nice", "-n", "5", "rm", "x"]),
    ] {
        let call = command(program, &args);
        assert_eq!(
            config(json!({"deny":["Bash(rm *)"]})).decision(&call),
            Decision::Deny,
            "{program} {args:?}"
        );
        assert_eq!(
            config(json!({"ask":["Bash(rm *)"], "allow":["Bash"]})).approval_mode(&call),
            Some(ApprovalMode::Request)
        );
        assert_eq!(
            config(json!({"allow":["Bash(rm *)"]})).decision(&call),
            Decision::Ask
        );
    }
    assert_eq!(
        config(json!({"deny":["Bash(rm *)"]})).decision(&command("echo", &["confirm"])),
        Decision::Ask
    );
}

#[test]
fn unguarded_commands_match_bash_deny_and_ask_but_never_allow() {
    let unguarded = ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec!["-c".into(), "cargo test".into()],
        cwd: String::new(),
        justification: "Run the failed command once".into(),
        prefix_rule: None,
    };
    let guarded = command("sh", &["-c", "cargo test"]);
    let allow = config(json!({"allow": ["Bash", "command_unguarded", "Bash(sh -c *)"]}));
    assert_eq!(allow.decision(&guarded), Decision::Allow);
    for mode in ["default", "acceptEdits", "auto"] {
        let parsed = config(json!({
            "defaultMode": mode,
            "allow": ["Bash", "command_unguarded", "Bash(sh -c *)"]
        }));
        assert_eq!(parsed.decision(&unguarded), Decision::Ask, "{mode}");
    }
    assert_eq!(
        config(json!({"defaultMode": "plan", "allow": ["Bash"]})).decision(&unguarded),
        Decision::Deny
    );
    assert_eq!(
        config(json!({"defaultMode": "dontAsk", "allow": ["Bash"]})).decision(&unguarded),
        Decision::Deny
    );
    let deny = config(json!({"deny": ["Bash"]})).evaluate(&unguarded);
    assert_eq!((deny.0, deny.1), (Decision::Deny, PermissionSource::Rule));
    let ask = config(json!({"ask": ["Bash(sh -c *)"]})).evaluate(&unguarded);
    assert_eq!((ask.0, ask.1), (Decision::Ask, PermissionSource::Rule));
    let bare_name = config(json!({"deny": ["command_unguarded"]})).evaluate(&unguarded);
    assert_eq!(bare_name.0, Decision::Deny);
    assert_eq!(
        config(json!({"deny": ["Bash"]})).decision(&guarded),
        Decision::Deny
    );
    assert_eq!(unguarded.label(), "command_unguarded");
    assert_eq!(peer_name(&unguarded), "Bash");
}

#[test]
fn auto_mode_never_routes_an_unguarded_command_to_ai_review() {
    let unguarded = ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec!["-c".into(), "cargo test".into()],
        cwd: String::new(),
        justification: "Run the failed command once".into(),
        prefix_rule: None,
    };
    let guarded = command("sh", &["-c", "cargo test"]);
    let parsed = config(json!({"defaultMode": "auto"}));
    assert_eq!(
        parsed.evaluate(&guarded).2,
        Some(crate::tools::ApprovalMode::Auto)
    );
    let evaluated = parsed.evaluate(&unguarded);
    assert_eq!(evaluated.0, Decision::Ask);
    assert_eq!(evaluated.2, Some(crate::tools::ApprovalMode::Request));
}

fn deny_with(reason: Option<&str>) -> HookResult {
    HookResult::Restricted {
        decision: Decision::Deny,
        reason: reason.map(str::to_owned),
    }
}

fn ignored(why: HookIgnoreReason) -> HookResult {
    HookResult::Ignored { why }
}

#[test]
fn hook_output_parsing_copies_peer_exit_codes_and_json_precedence() {
    let specific = |fields: Value| {
        let mut output = json!({"hookEventName": "PreToolUse"});
        output
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        json!({"hookSpecificOutput": output}).to_string()
    };
    let ask = HookResult::Restricted {
        decision: Decision::Ask,
        reason: Some("why".into()),
    };
    for (exit, stdout, stderr, expected) in [
        (Some(0), String::new(), "", HookResult::Passed),
        (Some(0), "plain text".into(), "", HookResult::Passed),
        (Some(0), "{broken".into(), "", HookResult::Passed),
        (Some(0), "{}".into(), "", HookResult::Passed),
        (
            Some(0),
            specific(json!({"permissionDecision": "deny", "permissionDecisionReason": "no"})),
            "",
            deny_with(Some("no")),
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "ask", "permissionDecisionReason": "why"})),
            "",
            ask,
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "allow"})),
            "",
            ignored(HookIgnoreReason::AllowIsNoOp),
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "defer"})),
            "",
            ignored(HookIgnoreReason::InvalidJson),
        ),
        (
            Some(0),
            json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "permissionDecision": "deny"}})
                .to_string(),
            "",
            ignored(HookIgnoreReason::InvalidJson),
        ),
        (
            Some(0),
            json!({"hookSpecificOutput": "deny"}).to_string(),
            "",
            ignored(HookIgnoreReason::InvalidJson),
        ),
        (
            Some(0),
            json!({"decision": "block", "reason": "stop"}).to_string(),
            "",
            deny_with(Some("stop")),
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "allow", "updatedInput": {"path": "/"}})),
            "",
            deny_with(Some("hook input update does not narrow")),
        ),
        (
            Some(0),
            specific(json!({"updatedInput": {"operation": "read", "path": "a", "offset": null, "limit": 1}})),
            "",
            HookResult::Narrowed { call: read("a") },
        ),
        (
            Some(0),
            specific(json!({"updatedInput": {"operation": "read", "path": "../a", "offset": null, "limit": 1}})),
            "",
            deny_with(Some("hook input update does not narrow")),
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "ask", "updatedInput": {"operation": "read", "path": "a", "offset": null, "limit": 1}})),
            "",
            HookResult::Restricted {
                decision: Decision::Ask,
                reason: None,
            },
        ),
        (
            Some(0),
            specific(json!({"permissionDecision": "deny", "updatedInput": {}})),
            "",
            deny_with(None),
        ),
        (Some(0), format!("{}{}", "[".repeat(64), "]".repeat(64)), "", HookResult::Passed),
        (
            Some(0),
            json!({"continue": false, "stopReason": "halt"}).to_string(),
            "",
            HookResult::Stopped { reason: Some("halt".into()) },
        ),
        (Some(2), "{}".into(), "blocked\nby policy", deny_with(Some("blockedby policy"))),
        (Some(2), String::new(), "", deny_with(None)),
        (Some(1), String::new(), "oops", ignored(HookIgnoreReason::NonZeroExit)),
        (None, String::new(), "", ignored(HookIgnoreReason::NonZeroExit)),
    ] {
        assert_eq!(
            parse_output(exit, &stdout, stderr),
            expected,
            "{exit:?} {stdout}"
        );
    }
    let hostile = specific(json!({
        "permissionDecision": "deny",
        "permissionDecisionReason": format!("\u{1b}[31m{}\u{7}\n", "é".repeat(2000)),
    }));
    let HookResult::Restricted {
        reason: Some(reason),
        ..
    } = parse_output(Some(0), &hostile, "")
    else {
        panic!("a deny carries its reason");
    };
    assert!(reason.len() <= MAX_HOOK_REASON_BYTES && reason.starts_with("[31m"));
    assert!(!reason.chars().any(char::is_control));
}

#[test]
fn hook_results_only_narrow_and_validate_against_their_observation() {
    assert_eq!(HookResult::Passed.restriction(&read("a")), Decision::Allow);
    let narrowed = HookResult::Narrowed { call: read("a") };
    assert_eq!(narrowed.restriction(&read("a")), Decision::Allow);
    assert_eq!(narrowed.restriction(&read("b")), Decision::Deny);
    assert_eq!(
        ignored(HookIgnoreReason::AllowIsNoOp).restriction(&read("a")),
        Decision::Allow
    );
    assert_eq!(deny_with(None).restriction(&read("a")), Decision::Deny);
    let allow = HookResult::Restricted {
        decision: Decision::Allow,
        reason: None,
    };
    let run = crate::session::RunId::new();
    let agent = crate::session::AgentRunId::new();
    let mut observation = crate::tools::ToolObservation {
        intent: crate::tools::EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: run,
            agent_run_id: agent,
            policy_digest: [1; 32],
            enforcement_digest: [1; 32],
            workspace_device: 1,
            workspace_inode: 1,
            call: command("sh", &["-c", "true"]),
            limits: crate::tools::ToolLimits::default(),
            use_count: 1,
        },
        disposition: crate::tools::ToolDisposition::Denied,
        enforcement_cause: None,
        limit_cause: None,
        output: "x".into(),
        attestation: None,
        approval_cause: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    };
    assert!(ignored(HookIgnoreReason::NotCompleted).valid_for(&observation));
    assert!(!HookResult::Passed.valid_for(&observation));
    observation.disposition = crate::tools::ToolDisposition::Limit;
    observation.limit_cause = Some(crate::tools::LimitCause::Resource);
    let limited = deny_with(Some("hook limit reached"));
    assert!(limited.valid_for(&observation));
    assert!(!HookResult::Passed.valid_for(&observation));
    assert!(!allow.valid_for(&observation));
    let result = |exit: i32, stdout: &str| {
        serde_json::to_string(&json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": exit, "stdout": stdout, "stderr": "no", "stdout_total":{"bytes":(stdout).len(),"lines":(stdout).lines().count()},"stderr_total":{"bytes":2,"lines":1},"changeset": {"files": []}}))
            .unwrap()
    };
    observation.disposition = crate::tools::ToolDisposition::Failed;
    observation.limit_cause = None;
    observation.attestation = Some(crate::tools::Attestation::for_site(&observation.intent));
    observation.output = result(2, "");
    assert!(deny_with(Some("no")).valid_for(&observation));
    assert!(!HookResult::Passed.valid_for(&observation));
    observation.output = result(0, "");
    assert!(HookResult::Passed.valid_for(&observation));
    assert!(!deny_with(Some("no")).valid_for(&observation));
    observation.output = "not CommandResult JSON".into();
    assert_eq!(
        HookResult::for_event(HookEvent::PreToolUse, &observation),
        ignored(HookIgnoreReason::NotCompleted)
    );
    assert!(ignored(HookIgnoreReason::NotCompleted).valid_for(&observation));
    let overlong = deny_with(Some(&"x".repeat(MAX_HOOK_REASON_BYTES + 1)));
    assert!(!overlong.valid_for(&observation));
}

#[test]
fn hook_stdin_is_closed_bounded_and_host_path_free() {
    let call = ToolCall::Write {
        path: "notes/a.txt".into(),
        expected_digest: None,
        content: "x".repeat(MAX_HOOK_INPUT_BYTES / 2),
    };
    let id = uuid::Uuid::now_v7();
    let input =
        pre_tool_use_input("session", PermissionMode::AcceptEdits, &call, id).expect("fits");
    let value: Value = serde_json::from_str(&input).unwrap();
    let mut fields: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    fields.sort();
    assert_eq!(
        fields,
        [
            "cwd",
            "hook_event_name",
            "permission_mode",
            "session_id",
            "tool_input",
            "tool_name",
            "tool_use_id"
        ]
    );
    assert_eq!(value["cwd"], "/workspace");
    assert_eq!(value["hook_event_name"], "PreToolUse");
    assert_eq!(value["permission_mode"], "acceptEdits");
    assert_eq!(value["tool_name"], "Write");
    assert_eq!(value["tool_input"]["path"], "notes/a.txt");
    assert_eq!(value["tool_use_id"], id.to_string());
    assert!(input.len() <= MAX_HOOK_INPUT_BYTES);
    assert!(!input.contains("permissions.json"));
    // A valid call that cannot fit is refused whole: tool_input is never cut, even with an empty response.
    let large = ToolCall::Write {
        path: "notes/a.txt".into(),
        expected_digest: None,
        content: "x".repeat(super::super::types::MAX_EDIT_CONTENT_BYTES),
    };
    assert!(large.valid());
    assert!(matches!(
        pre_tool_use_input("session", PermissionMode::AcceptEdits, &large, id),
        Err(ConfigError::Limit(_))
    ));
    assert!(matches!(
        super::hooks::post_tool_use_input(
            "session",
            PermissionMode::AcceptEdits,
            &large,
            id,
            json!(""),
            0
        ),
        Err(ConfigError::Limit(_))
    ));
}

#[test]
fn hook_matching_is_bounded_ordered_and_fails_closed() {
    let handlers = |count: usize| {
        let hooks: Vec<_> = (0..count)
            .map(|index| json!({"type": "command", "command": format!("h{index}")}))
            .collect();
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"hooks": {"PreToolUse": [
                {"matcher": "Read", "hooks": hooks},
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "other"}]},
            ]}}))
            .unwrap(),
        )
        .unwrap()
    };
    let eight = handlers(8);
    let matched = eight
        .matched_tool_hooks(HookEvent::PreToolUse, &read("a"))
        .unwrap();
    assert_eq!(matched.len(), 8);
    assert_eq!(matched[3].2.command(), "h3");
    assert_eq!((matched[3].0, matched[3].1), (0, 3));
    assert!(matches!(
        handlers(9).matched_tool_hooks(HookEvent::PreToolUse, &read("a")),
        Err(ConfigError::Limit(_))
    ));
    assert!(
        handlers(9)
            .matched_tool_hooks(HookEvent::PreToolUse, &write("a"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn post_tool_use_output_becomes_bounded_feedback_and_never_replaces_the_result() {
    use super::hooks::{parse_post_output, post_tool_use_input};
    let lines = |lines: &[&str]| HookResult::Feedback {
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
    };
    let post = |value: serde_json::Value| json!({"hookSpecificOutput": value}).to_string();
    for (exit, stdout, stderr, expected) in [
        (Some(0), String::new(), "", HookResult::Passed),
        (Some(0), json!({"continue":false,"stopReason":"halt\nnow","decision":"block","reason":"not feedback"}).to_string(), "", HookResult::Stopped { reason: Some("haltnow".into()) }),
        (Some(0), json!({"continue":false}).to_string(), "", HookResult::Stopped { reason: None }),
        (Some(0), json!({"continue":false,"stopReason":"é".repeat(2000)}).to_string(), "", HookResult::Stopped { reason: Some("é".repeat(MAX_HOOK_REASON_BYTES / 2)) }),
        (Some(2), String::new(), "bad\nfile", lines(&["badfile"])),
        (Some(2), String::new(), "", lines(&["blocked without a reason"])),
        (Some(1), String::new(), "x", ignored(HookIgnoreReason::NonZeroExit)),
        (
            Some(0),
            json!({"decision": "block", "reason": "no"}).to_string(),
            "",
            lines(&["no"]),
        ),
        (
            Some(0),
            post(json!({"hookEventName": "PostToolUse", "additionalContext": "ctx", "updatedToolOutput": "x"})),
            "",
            lines(&["ctx"]),
        ),
        (
            Some(0),
            json!({"decision": "block", "reason": "no", "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "ctx"}}).to_string(),
            "",
            lines(&["no", "ctx"]),
        ),
        (
            Some(0),
            post(json!({"hookEventName": "PostToolUse", "updatedToolOutput": "x"})),
            "",
            HookResult::Passed,
        ),
        (
            Some(0),
            post(json!({"hookEventName": "PreToolUse", "additionalContext": "ctx"})),
            "",
            ignored(HookIgnoreReason::InvalidJson),
        ),
        (
            Some(0),
            json!({"hookSpecificOutput": {"permissionDecision": "allow"}}).to_string(),
            "",
            ignored(HookIgnoreReason::InvalidJson),
        ),
    ] {
        assert_eq!(parse_post_output(exit, &stdout, stderr), expected, "{stdout}");
    }
    let HookResult::Feedback { lines: long } = parse_post_output(Some(2), "", &"é".repeat(2000))
    else {
        panic!("exit 2 feeds back");
    };
    assert!(long[0].len() <= MAX_HOOK_REASON_BYTES);

    let many = vec![
        HookResult::Feedback {
            lines: vec!["x".repeat(MAX_HOOK_REASON_BYTES)]
        };
        8
    ];
    let rendered = super::hooks::feedback_lines(&many);
    assert!(rendered.iter().map(String::len).sum::<usize>() <= super::MAX_HOOK_FEEDBACK_BYTES);
    assert!(
        rendered
            .iter()
            .all(|line| line.starts_with("hook feedback: "))
    );

    let call = write("a");
    let mode = PermissionMode::Default;
    let id = uuid::Uuid::now_v7();
    let small = post_tool_use_input(
        "s",
        mode,
        &call,
        id,
        json!({"output": "ok"}),
        MAX_HOOK_INPUT_BYTES,
    )
    .unwrap();
    assert!(small.contains("\"hook_event_name\":\"PostToolUse\"") && !small.contains("truncated"));
    let huge = json!({"output": "\"\n\\".repeat(MAX_HOOK_INPUT_BYTES)});
    let cut =
        post_tool_use_input("s", mode, &call, id, huge.clone(), MAX_HOOK_INPUT_BYTES).unwrap();
    assert!(cut.len() <= MAX_HOOK_INPUT_BYTES);
    let parsed: serde_json::Value = serde_json::from_str(&cut).unwrap();
    assert_eq!(parsed["tool_response"]["truncated"], true);
    let bare = post_tool_use_input("s", mode, &call, id, huge, 0).unwrap();
    assert!(bare.len() < cut.len() && bare.contains("\"text\":\"\""));
}

#[test]
fn updated_input_accepts_only_provable_narrowing() {
    let window = |path: &str, offset, limit| ToolCall::Read {
        path: path.into(),
        offset: Some(offset),
        limit: Some(limit),
    };
    let list = |path: &str| ToolCall::List { path: path.into() };
    let search = |path: &str, query: &str| ToolCall::Search {
        path: path.into(),
        query: query.into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset: 0,
        limit: 100,
    };
    let page = |path: &str, offset: u32, limit: u32| ToolCall::Search {
        path: path.into(),
        query: "x".into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset,
        limit,
    };
    let matching = |regex: bool, include: Option<&str>, case_sensitive: bool| ToolCall::Search {
        path: "src".into(),
        query: "x".into(),
        regex,
        include: include.map(str::to_owned),
        case_sensitive,
        offset: 0,
        limit: 100,
    };
    let filtered = |path: &str, include: &str| ToolCall::Search {
        path: path.into(),
        query: "x".into(),
        regex: false,
        include: Some(include.into()),
        case_sensitive: true,
        offset: 0,
        limit: 100,
    };
    let glob_call = |pattern: &str, path: &str, limit| ToolCall::Glob {
        pattern: pattern.into(),
        path: path.into(),
        limit,
    };
    let status = |path: &str| ToolCall::GitStatus { path: path.into() };
    let diff = |path: &str, staged| ToolCall::GitDiff {
        path: path.into(),
        base: None,
        head: None,
        staged,
    };
    let log = |path: &str, max_count| ToolCall::GitLog {
        path: path.into(),
        revision: None,
        max_count,
    };
    let mcp = |arguments: &str| ToolCall::McpCall {
        server: "s".into(),
        tool: "t".into(),
        schema_digest: "d".into(),
        arguments: arguments.into(),
    };
    let skill = |resource: Option<&str>| ToolCall::Skill {
        name: "n".into(),
        resource: resource.map(str::to_owned),
    };
    let default = ToolCall::Read {
        path: "a".into(),
        offset: None,
        limit: None,
    };
    let cases = [
        (multiedit("a"), multiedit("a/b"), false),
        (multiedit("a"), write("a"), false),
        (default.clone(), window("a", 1, 2000), true),
        (window("a", 1, 2000), default.clone(), true),
        (window("a", 1, 32), default, false),
        (window("a", 10, 100), window("a", 10, 100), true),
        (window("a", 10, 100), window("a", 20, 50), true),
        (window("a", 10, 100), window("a", 10, 1), true),
        (window("a", 10, 100), window("a", 109, 1), true),
        (window("a", 10, 100), window("a", 10, 101), false),
        (window("a", 10, 100), window("a", 9, 10), false),
        (window("a", 10, 100), window("a", 110, 1), false),
        (window("a", 10, 100), window("a", u32::MAX, u32::MAX), false),
        (window("a", 10, 100), window("b", 10, 1), false),
        (window("a/b", 1, 10), window("a", 1, 10), false),
        (window("a", 1, 10), window("A", 1, 10), false),
        (window("a", 1, 10), window("./a", 1, 10), false),
        (window("a/b", 1, 10), window("a/../a/b", 1, 10), false),
        (list("src"), list("src"), true),
        (list("src"), list("src/deep/er"), true),
        (list("."), list("src"), true),
        (list(""), list("src"), true),
        (list("src"), list("."), false),
        (list("src"), list("src2"), false),
        (list("src"), list("srcx/a"), false),
        (list("src"), list("src/../etc"), false),
        (list("src"), list("src/.."), false),
        (list("src"), list("../src"), false),
        (list("src"), list("/src"), false),
        (list("src"), list("SRC/a"), false),
        (list("src"), list("src//a"), false),
        (list("src"), list("link/a"), false),
        (list("src"), search("src", "x"), false),
        (search("src", "x"), search("src/a", "x"), true),
        (search("src", "x"), search("src", "x"), true),
        (search("src", "x"), search("src/a", "y"), false),
        (search("src", "x"), search("src", "x*"), false),
        (search("src", "x"), search("..", "x"), false),
        (search(".", "x"), search("a", "x"), true),
        (
            glob_call("*.rs", ".", 100),
            glob_call("*.rs", "src", 100),
            true,
        ),
        (
            glob_call("*.rs", "src", 100),
            glob_call("*.rs", "src", 10),
            true,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("*.rs", "src/a", 10),
            true,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("*.rs", "src", 11),
            false,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("**/*.rs", "src", 10),
            false,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("*.r?", "src", 10),
            false,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("*.rs", ".", 10),
            false,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("*.rs", "srcx", 10),
            false,
        ),
        (
            glob_call("*.rs", "src", 10),
            glob_call("[a].rs", "src", 10),
            false,
        ),
        (glob_call("*.rs", "src", 10), list("src"), false),
        (page("src", 10, 20), page("src", 10, 20), true),
        (page("src", 10, 20), page("src", 15, 15), true),
        (page("src", 10, 20), page("src", 29, 1), true),
        (page("src", 10, 20), page("src", 15, 16), false),
        (page("src", 10, 20), page("src", 9, 5), false),
        (page("src", 10, 20), page("src", 30, 1), false),
        (page("src", 10, 20), page("src", u32::MAX, 100), false),
        (search("src", "x"), page("src/a", 0, 50), true),
        (search("src", "x"), page("src/a", 5, 50), false),
        (page("src", 5, 50), page("src/a", 0, 50), false),
        (page("src", 0, 50), page("src/a", 0, 51), false),
        (page("src/a", 0, 50), page("src", 0, 50), false),
        (
            matching(true, Some("**/*.rs"), false),
            matching(true, Some("**/*.rs"), false),
            true,
        ),
        (
            matching(false, None, true),
            matching(true, None, true),
            false,
        ),
        (
            matching(true, None, true),
            matching(false, None, true),
            false,
        ),
        (
            matching(false, None, true),
            matching(false, Some("*.rs"), true),
            false,
        ),
        (
            matching(false, Some("*.rs"), true),
            matching(false, None, true),
            false,
        ),
        (
            matching(false, Some("*.rs"), true),
            matching(false, Some("*.r?"), true),
            false,
        ),
        (
            matching(false, None, true),
            matching(false, None, false),
            false,
        ),
        (
            matching(false, None, false),
            matching(false, None, true),
            false,
        ),
        // `*.rs` under `src` excludes `src/nested/a.rs`; rebased under `src/nested` it would match.
        (
            filtered("src", "*.rs"),
            filtered("src/nested", "*.rs"),
            false,
        ),
        (
            filtered("src", "**/*.rs"),
            filtered("src/nested", "**/*.rs"),
            false,
        ),
        (filtered("src", "*.rs"), filtered("src", "*.rs"), true),
        (status("a"), status("a/b"), true),
        (status("a/b"), status("a"), false),
        (status("a"), status("a/../b"), false),
        (diff("a", true), diff("a/b", true), true),
        (diff("a", true), diff("a/b", false), false),
        (log("a", 5), log("a/b", 5), true),
        (log("a", 5), log("a/b", 6), false),
        (status("a"), diff("a", false), false),
        (
            command("ls", &["-l", "-n"]),
            command("ls", &["-l", "-n"]),
            false,
        ),
        (command("ls", &["-l", "-n"]), command("ls", &["-l"]), false),
        (command("ls", &["-l"]), command("ls", &["-l", "-n"]), false),
        (mcp("{}"), mcp("{}"), false),
        (mcp("{\"a\":1,\"b\":2}"), mcp("{\"a\":1}"), false),
        (skill(None), skill(None), false),
        (skill(Some("a")), skill(Some("a")), false),
        (write("a"), write("a"), false),
        (
            ToolCall::Mkdir { path: "a".into() },
            ToolCall::Mkdir { path: "a/b".into() },
            false,
        ),
        (
            ToolCall::ParseAgentDefinition {
                index: 0,
                raw_digest: [1; 32],
            },
            ToolCall::ParseAgentDefinition {
                index: 0,
                raw_digest: [1; 32],
            },
            false,
        ),
    ];
    for (original, updated, expected) in cases {
        assert_eq!(
            narrows(&original, &updated),
            expected,
            "{original:?} -> {updated:?}"
        );
    }
}

#[test]
fn lifecycle_hook_inputs_and_results_are_bounded_and_fail_closed() {
    use super::hooks::{lifecycle_input, parse_lifecycle_output, stop_feedback};
    for event in [
        HookEvent::Stop,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
    ] {
        let input = lifecycle_input(
            "session",
            PermissionMode::Default,
            event,
            true,
            &"\"é".repeat(32 * 1024),
            "auto",
            MAX_HOOK_INPUT_BYTES,
        );
        assert!(input.len() <= MAX_HOOK_INPUT_BYTES);
        let value: serde_json::Value = serde_json::from_str(&input).unwrap();
        assert_eq!(value["cwd"], "/workspace");
        if event == HookEvent::PreCompact {
            assert_eq!(value["trigger"], "auto");
            assert_eq!(value["custom_instructions"], "");
        } else {
            assert_eq!(value["stop_hook_active"], true);
        }
    }
    for (exit, stdout, stderr, expected) in [
        (0, "", "", HookResult::Passed),
        (
            1,
            "",
            "ignored",
            HookResult::Ignored {
                why: HookIgnoreReason::NonZeroExit,
            },
        ),
        (
            2,
            "",
            "verify\n",
            HookResult::Blocked {
                reason: Some("verify".into()),
            },
        ),
        (
            0,
            r#"{"decision":"block","reason":"verify"}"#,
            "",
            HookResult::Blocked {
                reason: Some("verify".into()),
            },
        ),
        (
            0,
            r#"{"continue":false,"stopReason":"halt","decision":"block","reason":"verify"}"#,
            "",
            HookResult::Stopped {
                reason: Some("halt".into()),
            },
        ),
    ] {
        assert_eq!(parse_lifecycle_output(Some(exit), stdout, stderr), expected);
    }
    let result = HookResult::Blocked {
        reason: Some("é".repeat(MAX_HOOK_REASON_BYTES / 2)),
    };
    let feedback = stop_feedback(std::iter::repeat_n(&result, 9));
    assert!(feedback.len() <= MAX_STOP_FEEDBACK_BYTES);
}

fn commit_call() -> ToolCall {
    ToolCall::GitCommit {
        message: "m".into(),
        paths: vec!["src/lib.rs".into()],
        staged: false,
    }
}

fn branch_call(name: Option<&str>) -> ToolCall {
    ToolCall::GitBranch {
        name: name.map(Into::into),
        start_point: None,
        scope: None,
        base: None,
        max_count: None,
    }
}

fn typed(verb: &[&str], decision: &str, workspace: Value) -> Value {
    json!({"prefix": verb, "decision": decision, "workspace": workspace})
}

fn typed_config(mode: &str, rules: Vec<Value>) -> Result<PermissionConfig, ConfigError> {
    PermissionConfig::parse(
        &serde_json::to_vec(&json!({"permissions": {"defaultMode": mode, "typedPrefixes": rules}}))
            .unwrap(),
    )
}

#[test]
fn typed_prefix_rules_validate_tokens_scopes_and_shared_bounds() {
    let folder = json!({"path": "/project", "device": 1, "inode": 2});
    for verb in [
        &["git", "commit"][..],
        &["git", "branch"],
        &["git", "switch"],
        &["git", "push"],
        &["gh", "pr", "create"],
    ] {
        typed_config("default", vec![typed(verb, "allow", Value::Null)]).unwrap();
        typed_config("default", vec![typed(verb, "deny", folder.clone())]).unwrap();
    }
    for bad in [
        typed(&["git"], "allow", Value::Null),
        typed(&["git", "commit", "-a"], "allow", Value::Null),
        typed(&["git", "commit", "extra", "more"], "allow", Value::Null),
        typed(&["git", "comm"], "allow", Value::Null),
        typed(&["git", "co*"], "allow", Value::Null),
        typed(&["/usr/bin/git", "commit"], "allow", Value::Null),
        typed(&["env", "git", "commit"], "allow", Value::Null),
        typed(&["git commit"], "allow", Value::Null),
        typed(&["git", "commit"], "maybe", Value::Null),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": "project", "device": 1, "inode": 2}),
        ),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": "/a/../b", "device": 1, "inode": 2}),
        ),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": "/a//b", "device": 1, "inode": 2}),
        ),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": "/a\nb", "device": 1, "inode": 2}),
        ),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": "/project", "device": 1, "inode": 0}),
        ),
        typed(
            &["git", "commit"],
            "allow",
            json!({"path": format!("/{}", "a".repeat(4096)), "device": 1, "inode": 2}),
        ),
        json!({"prefix": ["git", "commit"], "decision": "allow"}),
        json!({"prefix": ["git", "commit"], "decision": "allow", "workspace": null, "extra": 1}),
    ] {
        assert!(typed_config("default", vec![bad.clone()]).is_err(), "{bad}");
    }
    let same = typed(&["git", "commit"], "allow", Value::Null);
    assert!(typed_config("default", vec![same.clone(), same]).is_err());
    typed_config(
        "default",
        vec![
            typed(&["git", "commit"], "allow", Value::Null),
            typed(&["git", "commit"], "deny", Value::Null),
        ],
    )
    .unwrap();

    let ordinary = |count: usize| {
        (0..count)
            .map(|index| format!("Read(a{index})"))
            .collect::<Vec<_>>()
    };
    let typed_rows = |count: usize| {
        (0..count)
            .map(|index| {
                typed(
                    &["git", "commit"],
                    "allow",
                    json!({"path": format!("/p{index}"), "device": 1, "inode": 2}),
                )
            })
            .collect::<Vec<_>>()
    };
    let parse = |ordinary_count: usize, typed_count: usize| {
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions": {
                "deny": ordinary(ordinary_count),
                "typedPrefixes": typed_rows(typed_count),
            }}))
            .unwrap(),
        )
    };
    parse(100, 28).unwrap();
    assert!(matches!(parse(100, 29), Err(ConfigError::Limit(_))));
    assert!(matches!(parse(0, 129), Err(ConfigError::Limit(_))));
}

#[test]
fn typed_prefix_classification_ignores_raw_commands_and_branch_listing() {
    let rules = vec![typed(&["git", "commit"], "deny", Value::Null)];
    let parsed = typed_config("default", rules).unwrap();
    assert_eq!(parsed.decision(&commit_call()), Decision::Deny);
    assert_eq!(
        parsed.decision(&command("git", &["commit", "-m", "m"])),
        Decision::Ask
    );
    let rules = vec![typed(&["git", "branch"], "deny", Value::Null)];
    let parsed = typed_config("default", rules).unwrap();
    assert_eq!(parsed.decision(&branch_call(Some("topic"))), Decision::Deny);
    assert_eq!(parsed.decision(&branch_call(None)), Decision::Allow);
    let rules = vec![
        typed(&["git", "push"], "allow", Value::Null),
        typed(&["gh", "pr", "create"], "allow", Value::Null),
    ];
    let parsed = typed_config("default", rules).unwrap();
    assert_eq!(parsed.decision(&commit_call()), Decision::Ask);
    assert_eq!(parsed.decision(&command("git", &["push"])), Decision::Ask);
}

#[test]
fn typed_prefix_scope_binds_folder_identity_and_ignores_unscoped_allows() {
    let here = json!({"path": "/project", "device": 1, "inode": 2});
    let elsewhere = json!({"path": "/other", "device": 1, "inode": 2});
    let moved = json!({"path": "/project", "device": 1, "inode": 3});
    let identity = crate::session::WorkspaceIdentity {
        device: 1,
        inode: 2,
    };
    let allow = |scope: &Value| typed(&["git", "commit"], "allow", scope.clone());
    let deny = |scope: &Value| typed(&["git", "commit"], "deny", scope.clone());
    for (scope, applies) in [(&here, true), (&elsewhere, false), (&moved, false)] {
        let mut parsed = typed_config("default", vec![allow(scope)]).unwrap();
        assert_eq!(parsed.decision(&commit_call()), Decision::Ask, "unscoped");
        parsed.scope_to(Path::new("/project"), identity);
        assert_eq!(
            parsed.decision(&commit_call()),
            if applies {
                Decision::Allow
            } else {
                Decision::Ask
            },
            "{scope}"
        );
        let mut parsed = typed_config("default", vec![deny(scope)]).unwrap();
        assert_eq!(parsed.decision(&commit_call()), Decision::Deny, "unscoped");
        parsed.scope_to(Path::new("/project"), identity);
        assert_eq!(
            parsed.decision(&commit_call()),
            if applies {
                Decision::Deny
            } else {
                Decision::Ask
            },
            "{scope}"
        );
    }
    let mut parsed = typed_config("default", vec![allow(&Value::Null)]).unwrap();
    assert_eq!(parsed.decision(&commit_call()), Decision::Allow);
    parsed.scope_to(Path::new("/project"), identity);
    assert_eq!(parsed.decision(&commit_call()), Decision::Allow);
}

#[test]
fn typed_prefix_rules_obey_modes_and_restrictive_precedence() {
    use Decision::{Allow, Ask, Deny};
    let call = commit_call();
    // [no rule, allow, ask, deny] per mode.
    for (mode, expected) in [
        ("default", [Ask, Allow, Ask, Deny]),
        ("acceptEdits", [Ask, Ask, Ask, Deny]),
        ("auto", [Ask, Ask, Ask, Deny]),
        ("plan", [Deny, Deny, Deny, Deny]),
        ("dontAsk", [Deny, Allow, Deny, Deny]),
    ] {
        for (decision, want) in [None, Some("allow"), Some("ask"), Some("deny")]
            .into_iter()
            .zip(expected)
        {
            let rules = decision
                .map(|decision| vec![typed(&["git", "commit"], decision, Value::Null)])
                .unwrap_or_default();
            let parsed = typed_config(mode, rules).unwrap();
            assert_eq!(parsed.decision(&call), want, "{mode} {decision:?}");
        }
    }

    let allow = typed(&["git", "commit"], "allow", Value::Null);
    let ask = typed(&["git", "commit"], "ask", Value::Null);
    let parsed = typed_config("default", vec![allow.clone()]).unwrap();
    assert_eq!(
        parsed.decision_with_source(&call),
        (Allow, PermissionSource::TypedPrefixRule)
    );
    let parsed = typed_config("default", vec![ask.clone()]).unwrap();
    let (decision, source, approval) = parsed.evaluate(&call);
    assert_eq!(
        (decision, source, approval),
        (
            Ask,
            PermissionSource::TypedPrefixRule,
            Some(ApprovalMode::Request)
        )
    );
    let parsed = typed_config("auto", vec![ask]).unwrap();
    assert_eq!(parsed.evaluate(&call).2, Some(ApprovalMode::Request));
    let parsed = typed_config("auto", Vec::new()).unwrap();
    assert_eq!(parsed.evaluate(&call).2, Some(ApprovalMode::Auto));

    for restriction in ["Read(src/**)", "Edit(src/**)", "git_commit"] {
        for list in ["deny", "ask"] {
            let parsed = PermissionConfig::parse(
                &serde_json::to_vec(&json!({"permissions": {
                    list: [restriction],
                    "typedPrefixes": [allow.clone()],
                }}))
                .unwrap(),
            )
            .unwrap();
            let want = if list == "deny" { Deny } else { Ask };
            assert_eq!(parsed.decision(&call), want, "{list} {restriction}");
        }
    }
    let parsed = PermissionConfig::parse(
        &serde_json::to_vec(&json!({"permissions": {
            "allow": ["git_commit"],
            "typedPrefixes": [typed(&["git", "commit"], "deny", Value::Null)],
        }}))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        parsed.decision_with_source(&call),
        (Deny, PermissionSource::TypedPrefixRule)
    );
    let parsed = PermissionConfig::parse(
        &serde_json::to_vec(&json!({"permissions": {
            "deny": ["git_commit"],
            "typedPrefixes": [allow],
        }}))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        parsed.decision_with_source(&call),
        (Deny, PermissionSource::Rule)
    );
}

#[test]
fn a_typed_allow_keeps_its_provenance_when_an_ordinary_allow_also_matches() {
    let parsed = PermissionConfig::parse(
        &serde_json::to_vec(&json!({"permissions": {
            "allow": ["git_commit"],
            "typedPrefixes": [typed(&["git", "commit"], "allow", Value::Null)],
        }}))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        parsed.decision_with_source(&commit_call()),
        (Decision::Allow, PermissionSource::TypedPrefixRule)
    );
    let ordinary_only = PermissionConfig::parse(
        &serde_json::to_vec(&json!({"permissions": {"allow": ["git_commit"]}})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        ordinary_only.decision_with_source(&commit_call()),
        (Decision::Allow, PermissionSource::Rule)
    );
}

#[test]
fn host_git_modes_and_prefixes_never_replace_human_approval() {
    let calls = [
        ToolCall::GitPush {
            remote: "https://github.com/o/r.git".into(),
            branch: "main".into(),
        },
        ToolCall::GhPrCreate {
            repository: "github.com/o/r".into(),
            base: "main".into(),
            head: "topic".into(),
            title: "T".into(),
            body: String::new(),
            draft: false,
        },
    ];
    for mode in ["default", "acceptEdits", "auto", "plan", "dontAsk"] {
        for decision in ["allow", "ask", "deny"] {
            for call in &calls {
                let prefix = TypedGitPrefix::of_call(call).unwrap();
                let config =
                    typed_config(mode, vec![typed(prefix.tokens(), decision, Value::Null)])
                        .unwrap();
                let (verdict, _, approval) = config.evaluate(call);
                if decision == "deny" || matches!(mode, "plan" | "dontAsk") {
                    assert_eq!(verdict, Decision::Deny, "{mode} {decision}");
                    assert_eq!(approval, None);
                } else {
                    assert_eq!(verdict, Decision::Ask, "{mode} {decision}");
                    assert_eq!(approval, Some(ApprovalMode::Request));
                }
                let (channel, _) = crate::tools::ToolApprovals::new(ApprovalMode::Auto);
                assert!(!channel.without_human_only().admits_human_only(call, false));
            }
        }
    }
}

fn host_intent() -> crate::tools::EffectIntent {
    let mut pin = crate::tools::types::test_host_pin();
    pin.executable = "/usr/bin/printf".into();
    pin.permissions_digest = Some([7; 32]);
    crate::tools::EffectIntent {
        host: None,
        host_command: Some(pin),
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::RunId::new(),
        agent_run_id: crate::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::CommandUnguarded {
            program: "printf".into(),
            args: vec!["value %s".into(), "first".into()],
            cwd: String::new(),
            justification: "Inspect synthetic host data".into(),
            prefix_rule: None,
        },
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    }
}

#[test]
fn remembered_host_prefix_revalidates_executable_bytes_before_saving() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = crate::StateRoot::admit(&root.join("state")).unwrap();
    let tools = state.path().join("tools.json");
    std::fs::write(
        &tools,
        br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .unwrap();
    std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o600)).unwrap();
    let executable = root.join("inspect");
    std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut intent = host_intent();
    let identity = crate::tools::workspace_identity(&workspace).unwrap();
    intent.workspace_device = identity.device;
    intent.workspace_inode = identity.inode;
    let mut pin =
        crate::tools::unguarded::pin("inspect", &workspace, root.to_str().unwrap()).unwrap();
    pin.state_roots = [
        state.path().to_str().unwrap().into(),
        root.join("account").to_str().unwrap().into(),
    ];
    intent.host_command = Some(pin);
    let ToolCall::CommandUnguarded {
        program,
        prefix_rule,
        ..
    } = &mut intent.call
    else {
        unreachable!()
    };
    *program = "inspect".into();
    *prefix_rule = Some(vec![program.clone(), "value %s".into()]);
    let digest = PermissionSettings::remember_host(&state, &intent, None).unwrap();
    let file = state.path().join("permissions.json");
    let before = std::fs::read(&file).unwrap();
    let mut changed = std::fs::read(&executable).unwrap();
    changed.extend_from_slice(b"changed");
    std::fs::write(&executable, changed).unwrap();
    let ToolCall::CommandUnguarded {
        args, prefix_rule, ..
    } = &mut intent.call
    else {
        unreachable!()
    };
    args[0] = "changed %s".into();
    prefix_rule.as_mut().unwrap()[1].clone_from(&args[0]);
    assert!(matches!(
        PermissionSettings::remember_host(&state, &intent, Some(digest)),
        Err(SettingsError::Invalid)
    ));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let reopened = PermissionSettings::load(&state, &workspace).unwrap();
    assert_eq!(reopened.host_prefixes().len(), 1);
    assert_eq!(reopened.digest(), Some(digest));
}

#[test]
fn host_prefix_records_enforce_literal_scope_closed_policy_and_settings_bounds() {
    let intent = host_intent();
    let rule = HostPrefixRule::from_intent(&intent, 2).unwrap();
    let parsed = config(json!({"hostPrefixes":[rule]}));
    assert!(parsed.host_rule(&intent).is_some());
    for change in [
        "suffix",
        "prefix",
        "cwd",
        "workspace",
        "identity",
        "digest",
        "environment",
        "state",
        "shell",
    ] {
        let mut changed = intent.clone();
        let ToolCall::CommandUnguarded { args, cwd, .. } = &mut changed.call else {
            unreachable!()
        };
        match change {
            "suffix" => args[1] = "second".into(),
            "prefix" => args[0] = "different %s".into(),
            "cwd" => *cwd = "other".into(),
            "workspace" => changed.workspace_inode += 1,
            "identity" => changed.host_command.as_mut().unwrap().inode += 1,
            "digest" => changed.host_command.as_mut().unwrap().sha256 = "b".repeat(64),
            "environment" => changed.host_command.as_mut().unwrap().environment_digest = [8; 32],
            "state" => args[1] = "--output=/state/permissions.json".into(),
            "shell" => args[1] = "$(printf data)".into(),
            _ => unreachable!(),
        }
        let matched = parsed.host_rule(&changed);
        assert_eq!(
            matched.is_some_and(|rule| rule.matches(&changed)),
            change == "suffix",
            "{change}"
        );
    }
    for name in crate::tools::unguarded::PRIVILEGE_TOOLS
        .iter()
        .chain(crate::tools::unguarded::WRAPPERS)
        .chain(crate::tools::unguarded::INTERPRETERS)
        .chain(["git", "gh", "rm"].iter())
    {
        for resolved in [false, true] {
            let mut record = serde_json::to_value(&rule).unwrap();
            if resolved {
                record["pin"]["executable"] = json!(format!("/usr/bin/{name}"));
            } else {
                record["prefix"][0] = json!(format!("/usr/bin/{name}"));
            }
            assert!(
                PermissionConfig::parse(
                    &serde_json::to_vec(&json!({"permissions":{"hostPrefixes":[record]}})).unwrap()
                )
                .is_err(),
                "{name} resolved={resolved}"
            );
        }
    }
    let mut record = serde_json::to_value(&rule).unwrap();
    record["version"] = json!(crate::tools::unguarded::HOST_POLICY_VERSION + 1);
    assert!(
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions":{"hostPrefixes":[record]}})).unwrap()
        )
        .is_err()
    );
    let duplicate = json!({"permissions":{"hostPrefixes":[rule,rule]}});
    assert!(PermissionConfig::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    let oversized =
        json!({"permissions":{"hostPrefixes":[rule], "allow": vec!["Read"; MAX_RULES]}});
    assert!(PermissionConfig::parse(&serde_json::to_vec(&oversized).unwrap()).is_err());
    assert!(PermissionConfig::parse(&vec![b' '; MAX_CONFIG_BYTES + 1]).is_err());
}

#[test]
fn host_prefix_scope_normalizes_lexical_paths_without_admitting_directory_aliases() {
    let intent = host_intent();
    let rule = HostPrefixRule::from_intent(&intent, 2).unwrap();
    let identity = crate::session::WorkspaceIdentity {
        device: intent.workspace_device,
        inode: intent.workspace_inode,
    };
    let mut parsed = config(json!({"hostPrefixes": [rule]}));
    parsed.scope_to(Path::new("//test-workspace/./"), identity);
    assert!(parsed.host_rule(&intent).is_some());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("child")).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    assert!(crate::tools::workspace_identity(&workspace).is_ok());
    assert!(crate::tools::workspace_identity(&alias).is_err());
    assert!(crate::tools::workspace_identity(&workspace.join("child/..")).is_err());
}

#[test]
fn declared_interpreter_and_git_pins_exclude_renamed_host_programs() {
    let intent = host_intent();
    let rule = HostPrefixRule::from_intent(&intent, 2).unwrap();
    let mut config: crate::tools::config::Config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
    assert!(rule.admitted_by(&config));
    for name in ["python3.14", "git", "gh"] {
        let program: crate::tools::config::Program = serde_json::from_value(json!({"name":name,"executable":format!("/opt/bin/{name}"),"sha256":"a".repeat(64),"interpreter":name == "python3.14","inputs":[]})).unwrap();
        config.commands = vec![program.clone()];
        assert!(!rule.admitted_by(&config), "{name}");
        config.commands.clear();
        config.git = Some(program);
        assert!(!rule.admitted_by(&config), "typed pin {name}");
        config.git = None;
    }
}

#[test]
fn delete_and_move_rules_cover_both_paths_without_partial_allows() {
    let delete = ToolCall::Delete {
        path: "src/source".into(),
        expected_digest: "0".repeat(64),
    };
    let move_call = ToolCall::Move {
        source: "src/source".into(),
        destination: "dest/target".into(),
        expected_digest: "0".repeat(64),
    };
    assert_eq!(
        config(json!({"allow":["Edit(./src/**)"]})).decision(&delete),
        Decision::Allow
    );
    for path in ["src", "dest"] {
        for (list, expected) in [
            ("deny", Decision::Deny),
            ("ask", Decision::Ask),
            ("allow", Decision::Ask),
        ] {
            assert_eq!(
                config(json!({list:[format!("Edit(./{path}/**)")]})).decision(&move_call),
                expected
            );
        }
        assert_eq!(
            config(json!({"deny":[format!("Read(./{path}/**)")]})).decision(&move_call),
            Decision::Deny
        );
    }
    assert_eq!(
        config(json!({"allow":["Edit(**)"]})).decision(&move_call),
        Decision::Allow
    );
    assert_eq!(
        config(json!({"deny":["delete"]})).decision(&delete),
        Decision::Deny
    );
    assert_eq!(
        config(json!({"deny":["move"]})).decision(&move_call),
        Decision::Deny
    );
}

#[test]
fn typed_host_pr_list_allow_applies_in_every_mode() {
    let call = ToolCall::GhPrList {
        repository: "github.com/o/r".into(),
        state: crate::tools::types::DEFAULT_PR_STATE,
        base: None,
        head: None,
        limit: crate::tools::types::DEFAULT_PR_LIMIT,
    };
    for mode in ["default", "acceptEdits", "auto", "plan", "dontAsk"] {
        let config = typed_config(
            mode,
            vec![typed(TypedGitPrefix::PrList.tokens(), "allow", Value::Null)],
        )
        .unwrap();
        assert_eq!(
            config.evaluate(&call),
            (Decision::Allow, PermissionSource::TypedPrefixRule, None),
            "{mode}"
        );
        let (channel, _) = crate::tools::ToolApprovals::new(ApprovalMode::Auto);
        assert!(!channel.without_human_only().admits_human_only(&call, false));
    }
    let ordinary_allow = config(json!({"allow": ["gh_pr_list"]}));
    assert_eq!(ordinary_allow.evaluate(&call).0, Decision::Ask);
}

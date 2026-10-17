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
        offset: 0,
        limit: 1,
    }
}

fn write(path: &str) -> ToolCall {
    ToolCall::Write {
        path: path.into(),
        expected_digest: None,
        content: "x".into(),
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
    for (mode, decisions) in [
        ("default", [Decision::Allow, Decision::Ask, Decision::Ask]),
        (
            "acceptEdits",
            [Decision::Allow, Decision::Allow, Decision::Ask],
        ),
        ("auto", [Decision::Allow, Decision::Ask, Decision::Ask]),
        ("plan", [Decision::Allow, Decision::Deny, Decision::Deny]),
        ("dontAsk", [Decision::Allow, Decision::Deny, Decision::Deny]),
    ] {
        let parsed = config(json!({"defaultMode": mode}));
        for (call, expected) in [&reads, &edits, &process].into_iter().zip(decisions) {
            assert_eq!(parsed.decision(call), expected, "{mode} {call:?}");
        }
    }
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
        ToolCall::Mkdir {
            path: "private/a".into(),
        },
        ToolCall::Search {
            path: "private/a".into(),
            query: "x".into(),
        },
        ToolCall::List {
            path: "private/a".into(),
        },
    ] {
        assert_eq!(parsed.decision(&call), Decision::Deny, "{call:?}");
    }
    for (rule, call) in [
        ("Edit(./src/**)", write("src/a")),
        ("Write", write("src/a")),
        ("read(./src/**)", read("src/a")),
        (
            "Grep(./src/**)",
            ToolCall::Search {
                path: "src/a".into(),
                query: "x".into(),
            },
        ),
        ("Glob", ToolCall::List { path: "src".into() }),
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
    let exact = format!("Read({})", "a".repeat(MAX_PATTERN_BYTES - 6));
    assert!(
        PermissionConfig::parse(
            &serde_json::to_vec(&json!({"permissions":{"deny":[exact]}})).unwrap()
        )
        .is_ok()
    );
    let over = format!("Read({})", "a".repeat(MAX_PATTERN_BYTES - 5));
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
        assert!(group.matches(&"x".repeat(MAX_PATTERN_BYTES + 1)).is_err());
        for subject in ["Bash\n", "Bash\0", "Bash\u{1b}"] {
            assert!(group.matches(subject).is_err());
        }
        assert!(group.matches_tool(&read("bad\npath")).is_err());
    }
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

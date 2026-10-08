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
    let legacy_limits = json!({
        "result_bytes": MAX_TOOL_RESULT_BYTES,
        "runtime_ms": 60_000,
        "memory_bytes": MEMORY_BYTES,
        "max_processes": MAX_PROCESSES,
        "scratch_bytes": SCRATCH_BYTES,
        "network": "none"
    });
    let legacy: ToolLimits = serde_json::from_value(legacy_limits.clone()).unwrap();
    assert_eq!(legacy.resource_profile, ToolResourceProfile::LinuxKernel);
    assert_eq!(serde_json::to_value(&legacy).unwrap(), legacy_limits);
    assert_eq!(serde_json::to_vec(&legacy).unwrap(),
        br#"{"result_bytes":16384,"runtime_ms":60000,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"network":"none"}"#,
        "historical serialized limits preserve digest bytes");
    let mut native = intent.clone();
    native.limits = legacy.clone();
    let legacy_intent = native.digest();
    native.limits.resource_profile = ToolResourceProfile::MacosSupervised;
    assert!(native.valid());
    assert_ne!(
        native.digest(),
        legacy_intent,
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
        super::ToolError::GuardRejected("admission"),
        super::ToolError::GuardRejected("Workspace identity"),
        super::ToolError::GuardRejected("kernel capability"),
        super::ToolError::GuardRejected("handshake"),
        super::ToolError::GuardRejected("payload"),
        super::ToolError::GuardRejected("receipt"),
        super::ToolError::GuardRejected("resource attestation"),
        super::ToolError::GuardRejected("process-unit cleanup"),
        super::ToolError::GuardRejected("launcher reap"),
        super::ToolError::GuardRejected("stderr EOF"),
        super::ToolError::GuardRejected("namespace bootstrap"),
        super::ToolError::GuardRejected("native manager"),
        super::ToolError::GuardRejected("bounded control protocol"),
        super::ToolError::GuardRejected("arbitrary text is not a durable diagnostic"),
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
        assert!(!uncertain.output.contains("arbitrary text"));
    }
    let mut oversized = observation;
    oversized.output = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
    assert!(!oversized.valid());
}

use crate::session::WorkspaceIdentity;
use crate::tools::catalog::catalog_call_schemas;
use crate::tools::config::Config;
use crate::tools::skills::parse_json;
use crate::tools::tests::{config_value, in_process_runtime, inspect_read, run_in_process};
use crate::tools::types::*;
use crate::validate::hex_digest;
use serde_json::json;

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
        network: None,
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

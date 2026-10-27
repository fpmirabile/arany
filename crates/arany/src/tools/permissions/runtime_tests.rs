use crate::tools::config::Config;
use crate::tools::glob_tests::{glob, glob_paths, glob_result};
use crate::tools::search_tests::{search, search_page, with_search};
use crate::tools::tests::{config_value, in_process_runtime, run_in_process};
use crate::tools::types::*;
use crate::validate::digest;
use serde_json::{Value, json};

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

#[tokio::test]
async fn conflicting_host_rules_name_the_saved_entry_and_remain_removable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("note.txt"), "workspace fact").unwrap();
    let state = crate::store::StateRoot::admit(&root.join("state")).unwrap();
    let config = conflicting_host_rule_config(&state, &workspace);
    let file = state.path().join("permissions.json");
    let before = std::fs::read(&file).unwrap();
    let admit = || {
        super::ToolRuntime::admit(
            config.clone(),
            super::load_permissions(&state).unwrap(),
            None,
            &workspace,
            &state,
            root.join("missing-guard"),
        )
    };
    let error = admit()
        .err()
        .expect("the current folder has a conflicting rule");
    assert!(matches!(
        error,
        super::ToolError::HostPrefixConflict { rule: 2 }
    ));
    assert_eq!(
        super::failure_stage(&error),
        crate::tools::FailureStage::Admission
    );
    assert_eq!(
        error.to_string(),
        "Saved host command rule 2 conflicts with a declared interpreter or Git executable; open /permissions > Host command rules to review and remove it"
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let settings = super::permissions::PermissionSettings::load(&state, &workspace).unwrap();
    let removed = settings
        .save_host_edit(
            &state,
            &workspace,
            super::permissions::HostPrefixEdit::Remove { index: 1 },
        )
        .unwrap();
    assert_eq!(removed.host_prefixes().len(), 1);
    let runtime = admit().expect("removing the conflicting rule restores Tool admission");
    assert!(runtime.receipt().host_prefixes.is_empty());
    assert_eq!(
        run_in_process(
            &runtime,
            ToolCall::Read {
                path: "note.txt".into(),
                offset: None,
                limit: None,
            }
        )
        .await
        .disposition,
        ToolDisposition::Succeeded
    );
}

fn conflicting_host_rule_config(state: &crate::StateRoot, workspace: &std::path::Path) -> Config {
    use std::os::unix::fs::PermissionsExt;
    let root = workspace.parent().unwrap();
    let executable = root.join("inspect");
    std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut pin = super::unguarded::pin("inspect", workspace, root.to_str().unwrap()).unwrap();
    pin.state_roots = [
        state.path().to_str().unwrap().into(),
        root.join("account").to_str().unwrap().into(),
    ];
    let identity = super::workspace_identity(workspace).unwrap();
    let rule = json!({
        "version":super::unguarded::HOST_POLICY_VERSION,
        "prefix":["inspect","value"],"decision":"allow","pin":pin,
        "workspace_device":identity.device,"workspace_inode":identity.inode,"cwd":""
    });
    let mut other = rule.clone();
    other["pin"]["workspace"] = json!(root.join("other-workspace"));
    let before = serde_json::to_vec(&json!({"permissions":{"hostPrefixes":[other,rule]}})).unwrap();
    let file = state.path().join("permissions.json");
    std::fs::write(&file, &before).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    serde_json::from_value(json!({
        "version":1,"workspace_paths":["."],"write":false,"skills":[],"mcp":[],
        "commands":[{"name":"inspect","executable":executable,"sha256":pin.sha256,"interpreter":true,"inputs":[]}]
    })).unwrap()
}

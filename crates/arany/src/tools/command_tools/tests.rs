use super::*;
use crate::tools::toolchains::{GrantRoot, RuntimeKind, RuntimeManager};
use std::os::unix::fs::PermissionsExt;

fn fixture(root: &Path) -> (StateRoot, std::path::PathBuf, String, RuntimeGrant) {
    let workspace = root.join("workspace");
    let directory = root.join("programs");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let executable = directory.join("filter");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let metadata = directory.metadata().unwrap();
    let grant = RuntimeGrant {
        kind: RuntimeKind::UserPrograms,
        manager: RuntimeManager::UserDirectory,
        version: None,
        path: directory,
        device: metadata.dev(),
        inode: metadata.ino(),
        root: None,
    };
    let state = StateRoot::admit(&root.join("state")).unwrap();
    let metadata = workspace.metadata().unwrap();
    state
        .record(RecordKind::WorkspacePermissions)
        .replace(
            &serde_json::to_vec(&serde_json::json!({"version":2,"workspaces":[{
                "path":workspace,"device":metadata.dev(),"inode":metadata.ino(),
                "mode":"request","runtime_grants":[grant]
            }]}))
            .unwrap(),
        )
        .unwrap();
    (state, workspace, executable.to_str().unwrap().into(), grant)
}

#[test]
fn selection_is_private_disabled_until_probed_and_drift_requires_a_new_pin() {
    let temp = tempfile::tempdir().unwrap();
    let (state, workspace, executable, grant) = fixture(temp.path());
    assert_eq!(
        command_tool_candidates(&state, &workspace).unwrap(),
        std::slice::from_ref(&executable)
    );
    let tool = CommandTool::pin(&state, &workspace, &executable, vec!["hook".into()]).unwrap();
    CommandTools::edit(&state, |record| record.select(tool.clone())).unwrap();
    let before = CommandTools::load(&state).unwrap().digest();
    let mut invalid = tool.clone();
    invalid.enabled = true;
    assert!(CommandTools::edit(&state, |record| record.replace(0, &tool, invalid)).is_err());
    assert_eq!(CommandTools::load(&state).unwrap().digest(), before);
    let mut enabled = tool.clone();
    enabled.last_probe = Some(RewriteStage::Rewritten);
    enabled.enabled = true;
    CommandTools::edit(&state, |record| record.replace(0, &tool, enabled)).unwrap();
    assert_ne!(CommandTools::load(&state).unwrap().digest(), before);
    std::fs::write(&executable, "#!/bin/sh\nexit 9\n").unwrap();
    assert!(tool.verify(std::slice::from_ref(&grant)).is_err());
    assert!(!tool.pin_status(&state, &workspace));
    let replacement = CommandTool::pin(&state, &workspace, &executable, tool.args.clone()).unwrap();
    assert!(!replacement.enabled);
    assert_eq!(replacement.last_probe, None);
    assert_ne!(replacement.sha256, tool.sha256);
    let current = CommandTools::load(&state).unwrap().entries()[0].clone();
    CommandTools::edit(&state, |record| record.replace(0, &current, replacement)).unwrap();
    CommandTools::edit(&state, |record| record.remove(0)).unwrap();
    assert!(CommandTools::load(&state).unwrap().entries().is_empty());
    let mut record = CommandTools::default();
    for index in 0..MAX_COMMAND_TOOLS {
        let mut candidate = tool.clone();
        candidate.executable = format!("/programs/filter-{index}");
        candidate.selected_path = candidate.executable.clone();
        record.select(candidate).unwrap();
    }
    assert!(matches!(record.select(tool), Err(ToolError::Limit)));
}

#[test]
fn discovery_refuses_overflow_and_selection_confines_links_to_granted_roots() {
    let temp = tempfile::tempdir().unwrap();
    let (state, workspace, executable, grant) = fixture(temp.path());
    let alias = grant.path.join("alias");
    std::os::unix::fs::symlink("filter", &alias).unwrap();
    let pinned = CommandTool::pin(&state, &workspace, alias.to_str().unwrap(), Vec::new()).unwrap();
    assert_eq!(pinned.executable, executable);
    let outside = temp.path().join("outside");
    std::fs::write(&outside, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(CommandTool::pin(&state, &workspace, outside.to_str().unwrap(), Vec::new()).is_err());
    let escape = grant.path.join("escape");
    std::os::unix::fs::symlink("../outside", &escape).unwrap();
    assert!(CommandTool::pin(&state, &workspace, escape.to_str().unwrap(), Vec::new()).is_err());
    let mut expected = vec![executable.clone(), alias.to_str().unwrap().to_owned()];
    expected.sort();
    assert_eq!(
        command_tool_candidates(&state, &workspace).unwrap(),
        expected
    );
    for index in 0..MAX_COMMAND_TOOL_CANDIDATES {
        std::fs::write(grant.path.join(format!("entry-{index}")), b"").unwrap();
    }
    assert!(matches!(
        command_tool_candidates(&state, &workspace),
        Err(ToolError::Limit)
    ));
    let record = state.record(RecordKind::WorkspacePermissions);
    let mut value: serde_json::Value =
        serde_json::from_slice(&record.read().unwrap().unwrap()).unwrap();
    let metadata = workspace.metadata().unwrap();
    value["workspaces"][0]["runtime_grants"][0]["path"] = serde_json::json!(workspace);
    value["workspaces"][0]["runtime_grants"][0]["device"] = serde_json::json!(metadata.dev());
    value["workspaces"][0]["runtime_grants"][0]["inode"] = serde_json::json!(metadata.ino());
    record
        .replace(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    assert!(matches!(
        command_tool_candidates(&state, &workspace),
        Err(ToolError::Path)
    ));
}

#[test]
fn peer_output_only_rewrites_command_and_shell_quoting_preserves_explicit_argv() {
    let original = ToolCall::Command {
        program: "printf".into(),
        args: vec!["a b".into(), "it's".into(), "$(x)".into()],
        cwd: "src".into(),
        resource_class: None,
        timeout_ms: None,
    };
    assert_eq!(script(&original).unwrap(), "printf 'a b' 'it'\\''s' '$(x)'");
    let input: serde_json::Value = serde_json::from_str(&input(&original).unwrap()).unwrap();
    assert_eq!(input["tool_name"], "Bash");
    assert_eq!(input["tool_input"]["command"], script(&original).unwrap());
    for (output, expected) in [
        ("", Ok(None)),
        (r#"{"permissionDecision":"allow"}"#, Ok(None)),
        (
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","updatedInput":{"command":"printf rewritten"}}}"#,
            Ok(Some("printf rewritten".into())),
        ),
        (
            r#"{"hookSpecificOutput":{"updatedInput":{"command":false}}}"#,
            Err(RewriteStage::InvalidOutput),
        ),
        ("not json", Err(RewriteStage::InvalidOutput)),
    ] {
        assert_eq!(parse_output(Some(0), output), expected);
    }
    assert_eq!(parse_output(Some(2), "{}"), Err(RewriteStage::NonZeroExit));
    let requested = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["test".into()],
        cwd: "src".into(),
        resource_class: Some(crate::tools::ToolResourceClass::Build),
        timeout_ms: Some(600_000),
    };
    assert_eq!(
        rewritten(&requested, "cargo test".into()),
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "cargo test".into()],
            cwd: "src".into(),
            resource_class: Some(crate::tools::ToolResourceClass::Build),
            timeout_ms: Some(600_000),
        }
    );
    let host = ToolCall::CommandUnguarded {
        program: "printf".into(),
        args: vec!["original".into()],
        cwd: "src".into(),
        justification: "Inspect output".into(),
        prefix_rule: Some(vec!["printf".into(), "original".into()]),
    };
    assert!(matches!(
        rewritten(&host, "printf rewritten".into()),
        ToolCall::CommandUnguarded { program, cwd, prefix_rule: None, .. }
            if program == "sh" && cwd == "src"
    ));
    assert!(
        script(&ToolCall::List {
            path: String::new()
        })
        .is_none()
    );
}

#[test]
fn a_relative_program_link_can_reach_the_granted_prefix_and_pins_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let (state, workspace, _, mut grant) = fixture(temp.path());
    let prefix = temp.path().join("prefix");
    let bin = prefix.join("bin");
    let libexec = prefix.join("libexec");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&libexec).unwrap();
    let executable = libexec.join("filter");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("../libexec/filter", bin.join("filter")).unwrap();
    let metadata = bin.metadata().unwrap();
    grant.path = bin.clone();
    grant.device = metadata.dev();
    grant.inode = metadata.ino();
    let metadata = prefix.metadata().unwrap();
    grant.root = Some(GrantRoot {
        path: prefix,
        device: metadata.dev(),
        inode: metadata.ino(),
    });
    let record = state.record(RecordKind::WorkspacePermissions);
    let mut value: serde_json::Value =
        serde_json::from_slice(&record.read().unwrap().unwrap()).unwrap();
    value["workspaces"][0]["runtime_grants"] = serde_json::json!([grant]);
    record
        .replace(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    let candidates = command_tool_candidates(&state, &workspace).unwrap();
    assert_eq!(
        candidates,
        [bin.join("filter").to_str().unwrap().to_owned()]
    );
    let tool = CommandTool::pin(&state, &workspace, &candidates[0], Vec::new()).unwrap();
    assert_eq!(tool.executable, executable.to_str().unwrap());
    assert!(tool.pin_status(&state, &workspace));
    let replacement = libexec.join("replacement");
    std::fs::copy(&executable, &replacement).unwrap();
    std::fs::remove_file(bin.join("filter")).unwrap();
    std::os::unix::fs::symlink("../libexec/replacement", bin.join("filter")).unwrap();
    assert!(!tool.pin_status(&state, &workspace));
    let repinned = CommandTool::pin(&state, &workspace, &tool.selected_path, Vec::new()).unwrap();
    assert_eq!(repinned.executable, replacement.to_str().unwrap());
    assert!(!repinned.enabled);
}

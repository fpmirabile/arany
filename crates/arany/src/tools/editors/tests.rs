use super::*;

#[test]
fn editor_workspaces_attach_adopt_on_resume_and_sweep_unresumable_ones() {
    crate::test_support::isolate_account_root();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/lib.rs"), b"base").unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut runtime = crate::tools::tests::in_process_runtime(
        &workspace,
        serde_json::json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.cache_state = Some(std::sync::Arc::new(state.try_clone().unwrap()));
    let session = SessionId::new();
    let (first, resumed, other) = (AgentRunId::new(), AgentRunId::new(), AgentRunId::new());
    runtime
        .create_editor_workspace(session, first, None)
        .unwrap();
    let tree = runtime.editor_tree_path(first).unwrap();
    assert_eq!(std::fs::read(tree.join("src/lib.rs")).unwrap(), b"base");
    std::fs::write(tree.join("src/lib.rs"), b"edited").unwrap();
    assert_eq!(runtime.editor_changes(first, 8, 1024).unwrap().len(), 1);
    runtime.release_editor_workspace(session, first, false);
    runtime
        .create_editor_workspace(session, resumed, Some(first))
        .unwrap();
    let adopted = runtime.editor_tree_path(resumed).unwrap();
    assert_eq!(
        std::fs::read(adopted.join("src/lib.rs")).unwrap(),
        b"edited"
    );
    assert!(
        !runtime
            .session_editors(session)
            .unwrap()
            .join(first.to_string())
            .exists()
    );
    runtime
        .create_editor_workspace(session, other, None)
        .unwrap();
    std::fs::create_dir(runtime.session_editors(session).unwrap().join("unknown")).unwrap();
    runtime.sweep_editor_workspaces(session, |id| id == resumed);
    let left: Vec<_> = std::fs::read_dir(runtime.session_editors(session).unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, [resumed.to_string()]);
    runtime.release_editor_workspace(session, resumed, true);
    assert!(!adopted.exists());
}

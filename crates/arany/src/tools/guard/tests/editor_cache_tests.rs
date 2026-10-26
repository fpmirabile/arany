use super::*;

#[test]
fn editor_commands_exclude_other_cache_writers_and_clear_then_release_for_reuse() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (state, _) = cache_fixture(temp.path());
    let workspace = temp.path().join("workspace");
    let mut runtime = crate::tools::tests::in_process_runtime(
        &workspace,
        serde_json::json!({
            "version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]
        }),
    );
    runtime.cache_state = Some(Arc::new(state.try_clone().unwrap()));
    let mut editor = runtime.intent(
        crate::RunId::new(),
        crate::AgentRunId::new(),
        ToolCall::Command {
            program: "build".into(),
            args: vec![],
            cwd: ".".into(),
        },
    );
    let (view_bytes, candidate_bytes) = runtime
        .effective_cache_bytes(editor.limits.cache_requested_bytes)
        .unwrap();
    editor.limits.cache_bytes = view_bytes;
    editor.limits.cache_candidate_bytes = candidate_bytes;
    let lease = build_cache_session(&runtime, &editor).unwrap().unwrap();
    for _ in 0..2 {
        let mut competing = editor.clone();
        competing.id = uuid::Uuid::now_v7();
        competing.agent_run_id = crate::AgentRunId::new();
        assert!(matches!(
            build_cache_session(&runtime, &competing),
            Err(ToolError::QueueFull)
        ));
    }
    assert!(matches!(
        state.clear_workspace_build_cache(&workspace),
        Err(StoreError::BuildCacheBusy)
    ));
    assert!(matches!(
        state.clear_all_build_caches(),
        Err(StoreError::BuildCacheBusy)
    ));
    drop(lease);
    state.clear_workspace_build_cache(&workspace).unwrap();
    let next = build_cache_session(&runtime, &editor).unwrap().unwrap();
    drop(next);
    state.clear_all_build_caches().unwrap();
    assert!(build_cache_session(&runtime, &editor).unwrap().is_some());
}

use super::*;

#[test]
fn editor_build_jobs_bind_selected_rust_before_guard_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let version = "1.98.1-x86_64-unknown-linux-gnu";
    let mut runtime = crate::tools::tests::in_process_runtime(
        directory.path(),
        serde_json::json!({
            "version":1,"workspace_paths":["."],"write":true,"skills":[],"mcp":[],
            "commands":[{"name":"sh","executable":"/unavailable","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}],
            "runtime_grants":[{"kind":"rust","manager":"rustup","version":version,"path":directory.path().join(".rustup/toolchains").join(version),"device":1,"inode":1}]
        }),
    );
    runtime.process_enforcement_digest =
        enforcement_digest(&runtime.config.runtime_grants).unwrap();
    let mut intent = runtime.intent(
        crate::RunId::new(),
        crate::AgentRunId::new(),
        ToolCall::Command {
            program: "sh".into(),
            args: Vec::new(),
            cwd: ".".into(),
            resource_class: None,
            timeout_ms: None,
        },
    );
    intent.limits.resource_class = crate::tools::ToolResourceClass::Build;
    intent.limits.build_jobs = 2;
    intent.limits.cache_bytes = 0;
    intent.limits.cache_candidate_bytes = 0;
    runtime.bind_build_jobs(&mut intent).unwrap();
    assert_ne!(
        intent.enforcement_digest,
        runtime.process_enforcement_digest
    );
    let mut forged = intent.clone();
    forged.enforcement_digest = runtime.process_enforcement_digest;
    assert!(matches!(
        runtime.pinned_root(&forged),
        Err(ToolError::Configuration)
    ));
    assert!(runtime.pinned_root(&intent).is_ok());
}

#[tokio::test]
async fn editor_commands_exclude_other_cache_writers_and_clear_then_release_for_reuse() {
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
            resource_class: None,
            timeout_ms: None,
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
    #[cfg(target_os = "linux")]
    for class in [ToolResourceClass::Default, ToolResourceClass::Build] {
        editor.limits.resource_class = class;
        editor.limits.cache_bytes = 0;
        editor.limits.cache_candidate_bytes = 0;
        for requested in [0, crate::store::BUILD_CACHE_ARCHIVE_BYTES] {
            editor.limits.cache_requested_bytes = requested;
            let admitted = cache_directory::session(&runtime, &editor, far_deadline()).await;
            if requested == 0 {
                assert!(admitted.unwrap().is_none());
            } else {
                assert!(matches!(admitted, Err(ToolError::Limit)));
            }
        }
    }
}

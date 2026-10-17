use super::*;
use std::os::unix::fs::PermissionsExt;

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn permission_dispatch_invalid_or_unsafe_settings_refuse_before_provider() {
    for case in ["malformed", "unknown", "public", "symlink", "oversized"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let engine = Engine::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let path = state.join("permissions.json");
        match case {
            "malformed" => private_file(&path, b"{"),
            "unknown" => private_file(&path, br#"{"permissions":{"unknown":true}}"#),
            "public" => {
                private_file(&path, b"{}");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            "symlink" => {
                let target = state.join("permissions-target.json");
                private_file(&target, b"{}");
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "oversized" => {
                let mut bytes = vec![b' '; 64 * 1024 - 1];
                bytes.extend_from_slice(b"{}");
                private_file(&path, &bytes);
            }
            _ => unreachable!(),
        }
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_outcome(
                arany::ToolCall::Read {
                    path: "notes/facts.txt".into(),
                    offset: 0,
                    limit: 32,
                },
                None,
            ),
            arany::test_support::goals::finish_with("answer"),
        ]));
        let mut input = request(&workspace, "Inspect the task");
        input.include_paths.clear();
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(
                result,
                Err(EngineError::Tool(arany::ToolError::Configuration))
            ),
            "{case}"
        );
        assert!(provider.requests.lock().unwrap().is_empty(), "{case}");
        engine.close().await.unwrap();
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let started: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started, 0, "{case}");
    }
}

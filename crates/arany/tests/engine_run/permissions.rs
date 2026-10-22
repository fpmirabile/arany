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
        let engine = engine_support::open(
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
            tool_outcome(arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
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

#[tokio::test]
async fn inactive_host_rules_enforce_file_admission_but_ignore_contents() {
    for (case, admitted) in [
        ("group-readable", false),
        ("symlink", false),
        ("hardlink", false),
        ("oversized", false),
        ("valid", true),
        ("malformed", true),
        ("129-entries", true),
        ("unknown", true),
        ("duplicate", true),
        ("version", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let path = state.join("unguarded-commands.json");
        let entry = serde_json::json!({
            "workspace": {"path":workspace, "device":1, "inode":2},
            "program":"sh", "args":["-c", "cargo test"], "cwd":"",
            "created_unix_ms":1
        });
        let mut record = serde_json::json!({"version":1, "commands":[entry.clone()]});
        match case {
            "unknown" => record["commands"][0]["extra"] = serde_json::json!(true),
            "duplicate" => record["commands"] = serde_json::json!([entry.clone(), entry]),
            "129-entries" => {
                record["commands"] = (0..129)
                    .map(|index| {
                        let mut entry = entry.clone();
                        entry["cwd"] = serde_json::json!(format!("dir{index}"));
                        entry
                    })
                    .collect();
            }
            "version" => record["version"] = serde_json::json!(2),
            _ => (),
        }
        private_file(&path, &serde_json::to_vec(&record).unwrap());
        match case {
            "group-readable" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            }
            "symlink" => {
                let target = state.join("rules-target.json");
                std::fs::rename(&path, &target).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "hardlink" => std::fs::hard_link(&path, state.join("rules-link.json")).unwrap(),
            "oversized" => private_file(&path, &vec![b' '; 64 * 1024 + 1]),
            "malformed" => private_file(&path, b"{"),
            _ => (),
        }
        let original = std::fs::read(&path).unwrap();
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
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
        if admitted {
            let outcome = result.unwrap();
            assert_eq!(outcome.run.status, RunStatus::Finished, "{case}");
            assert!(outcome.run.tools.is_empty(), "{case}");
            assert_eq!(provider.requests.lock().unwrap().len(), 1, "{case}");
        } else {
            assert!(
                matches!(
                    result,
                    Err(EngineError::Tool(arany::ToolError::Configuration))
                ),
                "{case}"
            );
            assert!(provider.requests.lock().unwrap().is_empty(), "{case}");
        }
        engine.close().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original, "{case}");
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let started: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started, i64::from(admitted), "{case}");
    }
}

#[tokio::test]
async fn run_started_does_not_pin_inactive_rules_before_or_after_edits() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
    let path = state.join("unguarded-commands.json");
    let original = br#"{"version":1,"commands":[]}"#;
    let edited = b"malformed inactive rules";
    private_file(&path, original);
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::goals::finish_with("first answer"),
        arany::test_support::goals::finish_with("second answer"),
    ]));
    let mut input = request(&workspace, "Inspect the task");
    input.include_paths.clear();
    let handle = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .unwrap();
    private_file(&path, edited);
    let first = handle.await.unwrap().run;
    let mut input = request(&workspace, "Inspect the task again");
    input.include_paths.clear();
    let second = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), edited);
    for outcome in [&first, &second] {
        let events = store.load_session(outcome.session_id).await.unwrap();
        let view = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        let run = view
            .runs
            .iter()
            .find(|run| run.id == outcome.run.id)
            .unwrap();
        assert_eq!(run, &outcome.run);
        assert_eq!(run.status, RunStatus::Finished);
        let policy = run.config.as_ref().unwrap().tool_policy.as_ref().unwrap();
        assert_eq!(policy.unguarded_rules_digest, None);
        assert!(events.iter().any(|event| matches!(&event.event,
            Event::RunStarted { config, .. }
                if config.tool_policy.as_ref().unwrap().unguarded_rules_digest.is_none()
        )));
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn agent_definition_unsafe_sources_refuse_before_provider() {
    for case in [
        "symlink",
        "hardlink",
        "public-private",
        "oversized",
        "linked-project-root",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let agents = state.join("agents");
        std::fs::create_dir(&agents).unwrap();
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = agents.join("reader.md");
        let valid = b"---\nname: reader\ndescription: Inspect\n---\nBody";
        match case {
            "symlink" => std::os::unix::fs::symlink(temp.path().join("outside"), &path).unwrap(),
            "hardlink" => {
                private_file(&temp.path().join("outside"), valid);
                std::fs::hard_link(temp.path().join("outside"), &path).unwrap();
            }
            "public-private" => {
                private_file(&path, valid);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            "oversized" => private_file(&path, &vec![b' '; 64 * 1024 + 1]),
            "linked-project-root" => {
                std::os::unix::fs::symlink(&agents, workspace.join(".claude")).unwrap()
            }
            _ => unreachable!(),
        }
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("forbidden egress reached a valid response"),
        ]));
        let mut input = request(&workspace, "Inspect");
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
                Err(EngineError::Tool(
                    arany::ToolError::Path | arany::ToolError::Limit
                ))
            ),
            "unsafe source rejected: {case}"
        );
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "no Provider disclosure"
        );
        engine.close().await.unwrap();
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn a_read_denied_skill_resource_is_denied_before_any_guard_task() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let skill = workspace.join(".agents/skills/x");
    std::fs::create_dir_all(skill.join("private")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\ndescription: Guide\n---\n").unwrap();
    std::fs::write(skill.join("private/key"), "DENY_CANARY\n").unwrap();
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(
        &state.join("tools.json"),
        br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    );
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"deny":["Read(./.agents/skills/x/private/**)"]}}"#,
    );
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(arany::ToolCall::Skill {
            name: "x".into(),
            resource: Some("private/key".into()),
        }),
        arany::test_support::goals::finish_with("done"),
    ]));
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Use the Skill"),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (events, _) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    let verdict = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolPermissionDecided { verdict, .. } => Some(verdict),
            _ => None,
        })
        .expect("Skill verdict");
    assert_eq!(
        (
            format!("{:?}", verdict.decision),
            format!("{:?}", verdict.basis)
        ),
        ("Deny".to_owned(), "Rule".to_owned())
    );
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
    assert!(observation.attestation.is_none(), "no Guard task ran");
    let requests = provider.requests.lock().unwrap();
    assert!(
        !format!("{:?}", requests.last().unwrap().input).contains("DENY_CANARY"),
        "the resource never reached the model"
    );
}

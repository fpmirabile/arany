use super::*;
use arany::test_support::scenario::{ScriptedProvider, request};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};

fn export(state: &Path, cwd: &Path, id: SessionId) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(cwd)
        .args(["show", "--output", "supervisor-corpus", "--state-dir"])
        .arg(state)
        .arg(id.to_string())
        .bounded_output()
        .expect("bounded corpus process")
}

#[tokio::test]
async fn supervisor_corpus_show_is_private_recomputed_and_export_local() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    let canaries = [
        "objective-canary-493",
        "path-canary-594",
        "argv-canary-695",
        "content-canary-796",
    ];
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join(canaries[1])).unwrap();
    std::fs::write(workspace.join(canaries[1]).join("data"), canaries[3]).unwrap();
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        arany::EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let executable = std::fs::canonicalize("/bin/echo").unwrap();
    let config = state.join("tools.json");
    std::fs::write(&config, serde_json::json!({"version":1,"workspace_paths":[canaries[1]],"write":true,"commands":[{"name":"synthetic","executable":executable,"sha256":hash(&std::fs::read(&executable).unwrap()),"interpreter":false,"inputs":[]}],"skills":[],"mcp":[]}).to_string()).unwrap();
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let permissions = state.join("permissions.json");
    std::fs::write(
        &permissions,
        br#"{"permissions":{"deny":["Bash","Write"]}}"#,
    )
    .unwrap();
    std::fs::set_permissions(permissions, std::fs::Permissions::from_mode(0o600)).unwrap();
    let read = arany::ToolCall::Read {
        path: format!("{}/data", canaries[1]),
        offset: 0,
        limit: 1024,
    };
    let mut outcomes: Vec<_> = (0..4).map(|_| tool_outcome(read.clone())).collect();
    outcomes.push(tool_outcome(arany::ToolCall::Command {
        program: "synthetic".into(),
        args: vec![canaries[2].into()],
        cwd: canaries[1].into(),
    }));
    outcomes.push(tool_outcome(arany::ToolCall::Write {
        path: format!("{}/new", canaries[1]),
        expected_digest: None,
        content: canaries[3].into(),
    }));
    outcomes.push(tool_outcome(read));
    outcomes.push(arany::ProviderOutcome::Finish(arany::Finish {
        summary: "done".into(),
        result: "done".into(),
    }));
    let provider = Arc::new(ScriptedProvider::new(outcomes));
    let mut input = request(&workspace, canaries[0]);
    input.include_paths.clear();
    input.goal.phase = Some(arany::GoalPhase::Implementation);
    let result = engine_support::run_with(
        &engine,
        &provider,
        input,
        arany::RunControls {
            tools: arany::ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 8);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(result.session_id).await.unwrap();
    let view = store.load_view(result.session_id).await.unwrap().unwrap();
    store.close().await.unwrap();
    let original = std::fs::read(state.join("events.sqlite3")).unwrap();
    let serialized = format!("{events:?}");
    for canary in canaries {
        assert!(serialized.contains(canary), "missing fixture canary");
    }
    assert_eq!(view.runs[0].supervisor.verdicts().len(), 3);
    arany::test_support::goals::assert_action_bounds(&events, false, false);
    std::fs::remove_dir_all(&workspace).unwrap();
    let mut exports = Vec::new();
    for _ in 0..2 {
        let output = export(&state, temp.path(), result.session_id);
        assert!(
            output.status.success(),
            "corpus exit: {:?}",
            output.status.code()
        );
        assert!(output.stderr.is_empty(), "corpus wrote stderr");
        let text = String::from_utf8(output.stdout).unwrap();
        for canary in canaries {
            assert!(!text.contains(canary), "corpus disclosed a canary");
        }
        for event in &events {
            assert!(!text.contains(&event.session_id.to_string()));
            if let Some(id) = event.run_id {
                assert!(!text.contains(&id.to_string()));
            }
            if let Some(id) = event.agent_run_id {
                assert!(!text.contains(&id.to_string()));
            }
        }
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["format"], "arany.supervisor.corpus.v1");
        assert_eq!(lines.len(), 8);
        for (i, line) in lines[1..5].iter().enumerate() {
            assert_eq!(line["relative_step"], i + 1);
            assert_eq!(line["features"]["loop"]["repeat_identical"], i + 1);
            assert_eq!(line["features"]["probing"]["denial_streak"], 0);
            assert_eq!(
                line["journaled_level"],
                match i {
                    2 => serde_json::json!("notice"),
                    3 => serde_json::json!("would_narrow"),
                    _ => Value::Null,
                }
            );
            assert_eq!(line["salted_call_key"], lines[1]["salted_call_key"]);
            assert_eq!(line["salted_outcome_key"], lines[1]["salted_outcome_key"]);
        }
        exports.push(lines);
    }
    for i in 1..exports[0].len() {
        for field in ["salted_call_key", "salted_outcome_key"] {
            assert_ne!(exports[0][i][field], exports[1][i][field]);
            exports[0][i].as_object_mut().unwrap().remove(field);
            exports[1][i].as_object_mut().unwrap().remove(field);
        }
    }
    assert_eq!(exports[0], exports[1]);
    assert!(
        std::fs::read(state.join("events.sqlite3")).unwrap() == original,
        "export changed journal bytes"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .open(state.join(format!("session-{}.lock", result.session_id)))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let locked = export(&state, temp.path(), result.session_id);
    assert!(!locked.status.success());
    assert!(locked.stdout.is_empty(), "locked export wrote stdout");
    assert_eq!(locked.stderr, b"error: Session has an active operation\n");
    drop(lock);
    assert!(
        export(&state, temp.path(), result.session_id)
            .status
            .success()
    );
    let lock_path = state.join(format!("session-{}.lock", result.session_id));
    for case in [
        "corrupt",
        "damaged",
        "missing",
        "symlink",
        "permissions",
        "hardlink",
    ] {
        let journal = state.join("events.sqlite3");
        match case {
            "corrupt" => {
                let db = rusqlite::Connection::open(&journal).unwrap();
                db.execute(
                    "UPDATE events SET payload='{}' WHERE kind='SupervisorVerdictRecorded'",
                    [],
                )
                .unwrap();
            }
            "damaged" => std::fs::write(&journal, b"not a SQLite journal").unwrap(),
            "missing" => std::fs::remove_file(&lock_path).unwrap(),
            "symlink" => {
                std::fs::remove_file(&lock_path).unwrap();
                std::os::unix::fs::symlink("absent-lock-target", &lock_path).unwrap();
            }
            "permissions" => {
                std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644))
                    .unwrap()
            }
            "hardlink" => std::fs::hard_link(&lock_path, state.join("lock-alias")).unwrap(),
            _ => unreachable!(),
        }
        let before_entries = directory_entries(temp.path());
        let before_journal = std::fs::read(&journal).unwrap();
        let output = export(&state, temp.path(), result.session_id);
        assert!(!output.status.success(), "{case}");
        assert!(
            output.stdout.is_empty(),
            "{case}: refused export wrote stdout"
        );
        let expected = match case {
            "corrupt" => "error: corrupt or unsupported Event history\n",
            "damaged" => "error: SQLite operation failed\n",
            "missing" => "error: invalid state directory\n",
            "symlink" => "error: state filesystem operation failed\n",
            "permissions" | "hardlink" => "error: state directory is not private\n",
            _ => unreachable!(),
        };
        assert_eq!(
            output.stderr,
            expected.as_bytes(),
            "{case}: refusal diagnostic"
        );
        assert_eq!(
            directory_entries(temp.path()),
            before_entries,
            "{case}: export created directory entries"
        );
        assert_eq!(
            std::fs::read(&journal).unwrap(),
            before_journal,
            "{case}: export changed journal"
        );
        match case {
            "corrupt" | "damaged" => std::fs::write(&journal, &original).unwrap(),
            "missing" | "symlink" => {
                if case == "symlink" {
                    std::fs::remove_file(&lock_path).unwrap();
                }
                std::fs::write(&lock_path, []).unwrap();
                std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            "permissions" => {
                std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap()
            }
            "hardlink" => std::fs::remove_file(state.join("lock-alias")).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            export(&state, temp.path(), result.session_id)
                .status
                .success(),
            "{case}: restored control"
        );
    }
}

#[test]
fn supervisor_corpus_is_a_show_output_only_and_validates_ids_before_state_access() {
    for (command, id, expected) in [
        (
            "show",
            Some("invalid"),
            "error: invalid Session or Run ID\n",
        ),
        ("exec", None, "error: invalid arguments; run arany --help\n"),
    ] {
        let mut process = Command::new(env!("CARGO_BIN_EXE_arany"));
        process
            .env_clear()
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([command, "--output", "supervisor-corpus"]);
        if let Some(id) = id {
            process.arg(id);
        }
        let output = process.bounded_output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, expected.as_bytes());
    }
}

fn directory_entries(root: &Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_owned()];
    let mut entries = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
            entries.push(entry.path());
        }
    }
    entries.sort();
    entries
}

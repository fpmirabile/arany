use arany::{
    AgentRole, AgentRunId, AgentStatus, CollaborationPolicy, ContextUsage, EngineError, Event,
    RunConfig, RunId, RunStatus, SessionDefaults, SessionId, SessionView, StateRoot, Store,
    StoreError, continue_session, create_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
use process::BoundedOutput;
use std::process::Command;

#[path = "common/process.rs"]
pub mod process;

#[cfg(target_os = "linux")]
#[path = "session_run/tools.rs"]
mod tools;

#[cfg(unix)]
#[path = "session_run/custom.rs"]
mod custom;

#[cfg(unix)]
#[path = "session_run/live.rs"]
mod live;

#[cfg(target_os = "linux")]
#[path = "session_run/agent_inspector.rs"]
mod agent_inspector;

#[cfg(target_os = "linux")]
#[path = "session_run/model_catalog.rs"]
mod model_catalog;

#[cfg(target_os = "linux")]
#[path = "session_run/slash_completion.rs"]
mod slash_completion;

#[cfg(target_os = "linux")]
#[path = "session_run/account_lock.rs"]
mod account_lock;
#[cfg(target_os = "linux")]
#[path = "session_run/setup.rs"]
mod setup;

#[cfg(unix)]
#[path = "session_run/loopback.rs"]
mod loopback;

#[cfg(target_os = "linux")]
#[path = "session_run/startup_trust.rs"]
mod startup_trust;

#[cfg(target_os = "linux")]
#[path = "session_run/active_terminal.rs"]
mod active_terminal;

#[cfg(target_os = "linux")]
#[path = "session_run/session_picker.rs"]
mod session_picker;

#[cfg(target_os = "linux")]
#[path = "session_run/auto_compaction.rs"]
mod auto_compaction;

#[cfg(target_os = "linux")]
#[path = "session_run/telemetry.rs"]
mod telemetry;

#[cfg(target_os = "linux")]
#[path = "session_run/disk_fault.rs"]
mod disk_fault;

#[cfg(target_os = "linux")]
#[path = "session_run/performance.rs"]
mod performance;

#[test]
fn version_one_journal_migrates_without_changing_canonical_events() {
    let temp = tempfile::tempdir().expect("private test root");
    let state_path = temp.path().join("state");
    StateRoot::admit(&state_path).expect("state root");
    let session_id = SessionId::new();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("version one database");
    connection
        .execute_batch(
            "CREATE TABLE events (
                sequence INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                run_id TEXT,
                agent_run_id TEXT,
                kind TEXT NOT NULL,
                event_version INTEGER NOT NULL CHECK (event_version >= 1),
                payload TEXT NOT NULL CHECK (json_valid(payload)),
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX events_by_session ON events (session_id, sequence);
            CREATE INDEX events_by_run ON events (run_id, sequence) WHERE run_id IS NOT NULL;
            PRAGMA user_version=1;",
        )
        .expect("version one schema");
    connection
        .execute(
            "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
             VALUES (1, ?1, 'SessionStarted', 1, '{\"title\":\"Legacy Session\"}', 1)",
            [session_id.to_string()],
        )
        .expect("version one Event");
    drop(connection);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let read_only =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("existing state"))
                .expect("legacy read-only Store");
        assert_eq!(read_only.load_session(session_id).await.unwrap().len(), 1);
        read_only.close().await.expect("close read-only Store");

        let store = Store::open(StateRoot::open_existing(&state_path).expect("existing state"))
            .expect("migrated Store");
        let events = store
            .load_session(session_id)
            .await
            .expect("preserved Events");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            Event::SessionStarted { title, workspace_identity: None } if title == "Legacy Session"
        ));
        store.close().await.expect("close migrated Store");
    });

    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect migrated database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 3);
    let table: String = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='provider_evidence'",
            [],
            |row| row.get(0),
        )
        .expect("evidence table");
    assert_eq!(table, "provider_evidence");
    let native_table: String = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='native_provider_evidence'",
            [],
            |row| row.get(0),
        )
        .expect("native evidence table");
    assert_eq!(native_table, "native_provider_evidence");
}

#[test]
fn version_two_journal_migrates_without_losing_custom_evidence() {
    let temp = tempfile::tempdir().expect("private test root");
    let state_path = temp.path().join("state");
    let root = StateRoot::admit(&state_path).expect("state root");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime
        .block_on(Store::open(root).expect("Store").close())
        .expect("close Store");
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("version two database");
    connection
        .execute_batch(
            "DROP TABLE native_provider_evidence;
             INSERT INTO provider_evidence (profile_name, profile_digest, addresses, checked_at_ms, expires_at_ms)
             VALUES ('legacy', zeroblob(32), '[\"127.0.0.1:443\"]', 1, 2);
             PRAGMA user_version=2;",
        )
        .expect("version two schema");
    drop(connection);

    runtime.block_on(async {
        let read_only =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("existing state"))
                .expect("legacy read-only Store");
        read_only.close().await.expect("close read-only Store");
        let store = Store::open(StateRoot::open_existing(&state_path).expect("existing state"))
            .expect("migrated Store");
        store.close().await.expect("close migrated Store");
    });

    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect migrated database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 3);
    let retained: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_evidence WHERE profile_name = 'legacy'",
            [],
            |row| row.get(0),
        )
        .expect("custom evidence survives migration");
    assert_eq!(retained, 1);
}

#[test]
fn attached_command_rejects_pipes_before_creating_state() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "hello"])
        .bounded_output()
        .expect("attached process");
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"error: attached mode requires terminal stdin and stderr; use arany exec for pipes\n"
    );
    assert!(!state_dir.exists());

    let picker = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--resume")
        .bounded_output()
        .expect("picker process");
    assert!(!picker.status.success());
    assert_eq!(picker.stdout, b"");
    assert_eq!(picker.stderr, output.stderr);
    assert!(!state_dir.exists());

    for flag in ["--screen-reader", "--no-color"] {
        let flagged = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--workspace")
            .arg(&workspace)
            .arg(flag)
            .bounded_output()
            .expect("flagged attached process");
        assert!(!flagged.status.success(), "{flag}");
        assert_eq!(flagged.stdout, b"", "{flag}");
        assert_eq!(flagged.stderr, output.stderr, "{flag}");
        assert!(!state_dir.exists(), "{flag}");
    }
}

#[test]
fn model_catalog_command_reads_only_selected_configuration() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let missing_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "openai"])
        .bounded_output()
        .expect("native catalog process");
    assert!(!missing_key.status.success());
    assert_eq!(missing_key.stdout, b"");
    assert_eq!(missing_key.stderr, b"error: Provider unavailable\n");
    assert!(!state_dir.exists());

    #[cfg(debug_assertions)]
    {
        let account_root = temp.path().join("missing-chatgpt-account");
        let chatgpt_missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args(["provider", "models", "chatgpt"])
            .bounded_output()
            .expect("missing ChatGPT account process");
        assert!(!chatgpt_missing.status.success());
        assert_eq!(chatgpt_missing.stdout, b"");
        assert_eq!(
            chatgpt_missing.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let missing_effort = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .current_dir(&workspace)
            .args([
                "exec",
                "--state-dir",
                state_dir.to_str().expect("test path"),
                "--provider",
                "chatgpt",
                "--model",
                "model-one",
                "objective",
            ])
            .bounded_output()
            .expect("ChatGPT Run requires effort before account access");
        assert!(!missing_effort.status.success());
        assert_eq!(missing_effort.stdout, b"");
        assert_eq!(
            missing_effort.stderr,
            b"error: ChatGPT requires --effort LEVEL\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let unselected_run = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "exec",
                "--state-dir",
                state_dir.to_str().expect("test path"),
                "--provider",
                "chatgpt",
                "--model",
                "model-one",
                "--effort",
                "high",
                "objective",
            ])
            .bounded_output()
            .expect("ChatGPT Run cannot fall back to API key");
        assert!(!unselected_run.status.success());
        assert_eq!(unselected_run.stdout, b"");
        assert_eq!(
            unselected_run.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let unchecked = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "provider",
                "check",
                "chatgpt",
                "model-one",
                "--effort",
                "high",
            ])
            .bounded_output()
            .expect("ChatGPT check requires explicit cost acceptance");
        assert!(!unchecked.status.success());
        assert_eq!(unchecked.stdout, b"");
        assert!(String::from_utf8_lossy(&unchecked.stderr).contains("no remote output-token cap"));
        assert!(!account_root.exists());

        let check_missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "provider",
                "check",
                "chatgpt",
                "model-one",
                "--effort",
                "high",
                "--accept-cost",
            ])
            .bounded_output()
            .expect("ChatGPT check with missing account");
        assert!(!check_missing.status.success());
        assert_eq!(check_missing.stdout, b"");
        assert_eq!(
            check_missing.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        for flag in ["--saved-account", "--state-dir"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .current_dir(&workspace)
                .args(["provider", "models", flag]);
            if flag == "--state-dir" {
                command.arg(&state_dir);
            }
            let result = command
                .arg("chatgpt")
                .bounded_output()
                .expect("invalid catalog flag");
            assert!(!result.status.success(), "{flag}");
            assert_eq!(result.stdout, b"", "{flag}");
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("omit --saved-account and --state-dir"),
                "{flag}"
            );
            assert!(!account_root.exists(), "{flag}");
        }

        for flag in ["--saved-account", "--state-dir"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .current_dir(&workspace)
                .args(["provider", "check", flag]);
            if flag == "--state-dir" {
                command.arg(&state_dir);
            }
            let result = command
                .args(["chatgpt", "model-one", "--effort", "high", "--accept-cost"])
                .bounded_output()
                .expect("invalid ChatGPT check flag");
            assert!(!result.status.success(), "{flag}");
            assert_eq!(result.stdout, b"", "{flag}");
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("omit --saved-account and --state-dir"),
                "{flag}"
            );
            assert!(!account_root.exists(), "{flag}");
            assert!(!state_dir.exists(), "{flag}");
        }
    }

    let custom_saved = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "--saved-account", "custom:local"])
        .bounded_output()
        .expect("invalid saved custom catalog process");
    assert!(!custom_saved.status.success());
    assert_eq!(custom_saved.stdout, b"");
    assert_eq!(
        custom_saved.stderr,
        b"error: --saved-account requires a native Provider\n"
    );
    assert!(!state_dir.exists());

    #[cfg(all(target_os = "linux", debug_assertions))]
    {
        let marker = temp.path().join("credential-command-marker");
        let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
        let unsafe_saved = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .current_dir(&workspace)
            .args(["provider", "models", "--saved-account", "openai"])
            .bounded_output()
            .expect("saved native catalog with unsafe transport");
        assert!(!unsafe_saved.status.success());
        assert_eq!(unsafe_saved.stdout, b"");
        assert_eq!(
            unsafe_saved.stderr,
            b"error: unsafe or unsupported D-Bus session address for the OS credential store\n"
        );
        assert!(!marker.exists());
    }

    let root = StateRoot::admit(&state_dir).expect("private state root");
    let profile_file = root.path().join("provider-profiles.json");
    std::fs::write(&profile_file, r#"{"version":1,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":1}]}"#).expect("profile file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&profile_file, std::fs::Permissions::from_mode(0o600))
            .expect("private profile file");
    }
    let custom = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "--state-dir"])
        .arg(&state_dir)
        .arg("custom:local")
        .bounded_output()
        .expect("custom catalog process");
    assert!(
        custom.status.success(),
        "{}",
        String::from_utf8_lossy(&custom.stderr)
    );
    assert_eq!(
        custom.stdout,
        b"Provider: custom:local\nModels: 1\nmodel-1 \xC2\xB7 exact profile; Run requires current conformance; effort provider default\n"
    );
    assert_eq!(custom.stderr, b"");
}

#[test]
fn native_model_check_requires_explicit_cost_consent_before_state_or_credentials() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let without_consent = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args(["openai", "example-model", "--effort", "high"])
        .bounded_output()
        .expect("native check without consent");
    assert!(!without_consent.status.success());
    assert_eq!(without_consent.stdout, b"");
    assert_eq!(
        without_consent.stderr,
        b"error: native checks may bill up to 3 inference calls and 3,072 generated tokens, plus bounded catalog/input usage; rerun with --accept-cost\n"
    );
    assert!(!state_dir.exists());

    let saved_without_consent = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args([
            "openai",
            "example-model",
            "--effort",
            "high",
            "--saved-account",
        ])
        .bounded_output()
        .expect("saved native check without consent");
    assert!(!saved_without_consent.status.success());
    assert_eq!(saved_without_consent.stdout, b"");
    assert_eq!(saved_without_consent.stderr, without_consent.stderr);
    assert!(!state_dir.exists());

    let invalid_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args(["openai", "bad\nmodel", "--effort", "high", "--accept-cost"])
        .bounded_output()
        .expect("native check with invalid model");
    assert!(!invalid_model.status.success());
    assert_eq!(invalid_model.stdout, b"");
    assert_eq!(
        invalid_model.stderr,
        b"error: native Provider, model, or effort is invalid\n"
    );
    assert!(!state_dir.exists());

    let without_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args([
            "openai",
            "example-model",
            "--effort",
            "high",
            "--accept-cost",
        ])
        .bounded_output()
        .expect("native check without key");
    assert!(!without_key.status.success());
    assert_eq!(without_key.stdout, b"");
    assert_eq!(
        without_key.stderr,
        b"error: selected native API key is unavailable or invalid\n"
    );
    assert!(state_dir.exists());

    #[cfg(all(target_os = "linux", debug_assertions))]
    {
        let marker = temp.path().join("credential-command-marker");
        let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
        let unsafe_saved_account = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .current_dir(&workspace)
            .args(["provider", "check", "--state-dir"])
            .arg(&state_dir)
            .args([
                "openai",
                "example-model",
                "--effort",
                "high",
                "--saved-account",
                "--accept-cost",
            ])
            .bounded_output()
            .expect("saved native check with unsafe transport");
        assert!(!unsafe_saved_account.status.success());
        assert_eq!(unsafe_saved_account.stdout, b"");
        assert_eq!(
            unsafe_saved_account.stderr,
            b"error: unsafe or unsupported D-Bus session address for the OS credential store\n"
        );
        assert!(!marker.exists());
    }
}

#[test]
fn exact_resume_and_rename_survive_file_backed_reopen() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state_path = temp.path().join("state");
        let id = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            None,
        )
        .await
        .expect("empty Session");
        let other_workspace = temp.path().join("other-workspace");
        std::fs::create_dir(&other_workspace).expect("other Workspace");
        assert!(matches!(
            resume_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                other_workspace.clone(),
                id,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state"),
                other_workspace,
                id,
                "Wrong Workspace".into(),
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state"),
                workspace.clone(),
                id,
                String::new(),
            )
            .await,
            Err(EngineError::InvalidRequest)
        ));
        rename_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            "Research thread".into(),
        )
        .await
        .expect("renamed Session");
        let view = resume_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace,
            id,
        )
        .await
        .expect("exact Session resume");
        assert_eq!(view.id, id);
        assert_eq!(view.title, "Research thread");
        assert!(view.runs.is_empty());
        assert_eq!(view.last_sequence, 2);
        assert!(view.workspace_identity.is_some());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1].event, Event::SessionRenamed { .. }));
        store.close().await.expect("store shutdown");
        let (device, inode) = view.workspace_identity.expect("bound Workspace");
        let shown = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &id.to_string()])
            .bounded_output()
            .expect("show process");
        assert!(shown.status.success());
        assert_eq!(shown.stderr, b"");
        let expected = format!(
            "{{\"sequence\":1,\"session_id\":\"{id}\",\"run_id\":null,\"agent_run_id\":null,\"kind\":\"SessionStarted\",\"event_version\":2,\"payload\":{{\"title\":\"New Session\",\"workspace_device\":{device},\"workspace_inode\":{inode}}},\"created_at_ms\":{}}}\n\
             {{\"sequence\":2,\"session_id\":\"{id}\",\"run_id\":null,\"agent_run_id\":null,\"kind\":\"SessionRenamed\",\"event_version\":1,\"payload\":{{\"title\":\"Research thread\"}},\"created_at_ms\":{}}}\n",
            events[0].created_at_ms, events[1].created_at_ms
        );
        assert_eq!(shown.stdout, expected.as_bytes());
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("version inspection");
        let version: i64 = connection
            .query_row("SELECT event_version FROM events WHERE sequence=1", [], |row| {
                row.get(0)
            })
            .expect("start Event version");
        assert_eq!(version, 2);
        connection
            .execute("UPDATE events SET event_version=1 WHERE sequence=1", [])
            .expect("version fault injection");
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen after corruption");
        assert!(matches!(
            store.load_view(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("store shutdown after corruption");
    });
}

#[test]
fn continue_selects_latest_committed_activity_in_the_admitted_workspace() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        let unused_workspace = temp.path().join("unused-workspace");
        for path in [&workspace, &other_workspace, &unused_workspace] {
            std::fs::create_dir(path).expect("Workspace");
        }
        let state_path = temp.path().join("state");
        let first = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            Some("First".into()),
        )
        .await
        .expect("first Session");
        let other = create_session(
            StateRoot::admit(&state_path).expect("state"),
            other_workspace.clone(),
            Some("Other Workspace".into()),
        )
        .await
        .expect("other Workspace Session");
        let second = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            Some("Second".into()),
        )
        .await
        .expect("second Session");
        let legacy = SessionId::new();
        let store =
            Store::open(StateRoot::admit(&state_path).expect("state")).expect("writable Store");
        store
            .append(
                legacy,
                Event::SessionStarted {
                    title: "Unbound legacy Session".into(),
                    workspace_identity: None,
                },
            )
            .await
            .expect("legacy Session");
        store.close().await.expect("Store shutdown");

        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("Workspace Session list");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(listed[0].title, "Second");
        assert!(listed[0].last_sequence > listed[1].last_sequence);
        assert!(
            list_sessions(
                StateRoot::open_existing(&state_path).expect("existing state"),
                unused_workspace.clone(),
            )
            .await
            .expect("empty Workspace list")
            .is_empty()
        );

        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("latest Workspace Session");
        assert_eq!(selected.id, second);
        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            other_workspace.clone(),
        )
        .await
        .expect("other Workspace Session");
        assert_eq!(selected.id, other);
        assert!(matches!(
            continue_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                unused_workspace,
            )
            .await,
            Err(EngineError::NoSessionForWorkspace)
        ));

        rename_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            first,
            "First, recently used".into(),
        )
        .await
        .expect("newest committed activity");
        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("latest by activity");
        assert_eq!(selected.id, first);
        assert_eq!(selected.title, "First, recently used");
        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("renamed Session list");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(listed[0].title, "First, recently used");

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET event_version=9 WHERE session_id=?1",
                [legacy.to_string()],
            )
            .expect("malformed unrelated Session");
        drop(connection);
        assert!(matches!(
            continue_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                workspace.clone(),
            )
            .await,
            Err(EngineError::Store(StoreError::InvalidHistory))
        ));
        assert!(matches!(
            list_sessions(
                StateRoot::open_existing(&state_path).expect("existing state"),
                workspace,
            )
            .await,
            Err(EngineError::Store(StoreError::InvalidHistory))
        ));
    });
}

#[test]
fn an_empty_session_and_defaults_are_durable_without_workspace_overlap() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state_path = temp.path().join("state");
        let id = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            None,
        )
        .await
        .expect("empty Session");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event, Event::SessionStarted { .. }));
        let view = store
            .load_view(id)
            .await
            .expect("resolved view")
            .expect("Session");
        assert_eq!(view.title, "New Session");
        assert!(view.runs.is_empty());
        assert!(view.workspace_identity.is_some());
        store.close().await.expect("closed read-only store");

        let defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: Some(arany::Effort::Medium),
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        set_session_defaults(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            defaults.clone(),
        )
        .await
        .expect("committed Session defaults");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1].event,
            Event::SessionDefaultChanged { .. }
        ));
        let view = store
            .load_view(id)
            .await
            .expect("resolved view")
            .expect("Session");
        assert_eq!(view.defaults, defaults);
        let (workspace_device, workspace_inode) =
            view.workspace_identity.expect("pinned Workspace identity");
        store.close().await.expect("closed read-only store");
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
        connection.execute("UPDATE events SET created_at_ms=CASE WHEN sequence=1 THEN 1791198000000 ELSE 1791288000000 END", []).unwrap();
        drop(connection);
        let listed = list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace.clone()).await.unwrap();
        assert_eq!(listed[0].title, "Empty conversation");
        assert_eq!(listed[0].created_at, "2026-10-05 11:00:00");
        assert_eq!(listed[0].last_activity_at, "2026-10-06 12:00:00");
        assert_eq!(listed[0].defaults, defaults);

        let invalid = set_session_defaults(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            SessionDefaults {
                provider: None,
                model: Some("gpt-5.4".into()),
                effort: None,
                account_id: None,
                policy: CollaborationPolicy::Single,
            },
        )
        .await;
        assert!(matches!(invalid, Err(EngineError::InvalidRequest)));
        let store =
            Store::open(StateRoot::admit(&state_path).expect("state")).expect("writable reopen");
        let pending_run_id = RunId::new();
        store
            .append(
                id,
                Event::MessageAccepted {
                    run_id: pending_run_id,
                    text: "pending objective".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("pending objective");
        let mut config = RunConfig {
            provider: "scripted".into(),
            model: "test-model".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device,
            workspace_inode,
            instruction_digest: None,
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: Some(ContextUsage {
                used_bytes: 101,
                budget_bytes: 100,
                compactable_bytes: 10,
                tool_history_bytes: 0,
            }),
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        };
        let legacy_config = serde_json::to_value(&config).expect("legacy Run config JSON");
        assert!(legacy_config.get("effort").is_none());
        assert_eq!(
            serde_json::from_value::<RunConfig>(legacy_config)
                .expect("legacy Run config")
                .effort,
            None
        );
        assert!(matches!(
            store
                .append(
                    id,
                    Event::RunStarted {
                        run_id: pending_run_id,
                        config: config.clone(),
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        config.context_usage = None;
        config.workspace_device = workspace_device.wrapping_add(1);
        assert!(matches!(
            store
                .append(
                    id,
                    Event::RunStarted {
                        run_id: pending_run_id,
                        config,
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed writable store");
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET payload=?1 WHERE sequence=2",
                [r#"{"provider":null,"model":"gpt-5.4","policy":{"mode":"single"}}"#],
            )
            .expect("invalid persisted defaults");
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        assert!(matches!(
            store.load_view(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed read-only store");

        let overlap_path = workspace.join("state");
        let overlap = create_session(
            StateRoot::admit(&overlap_path).expect("overlap state"),
            workspace,
            None,
        )
        .await;
        assert!(matches!(overlap, Err(EngineError::StateOverlap)));
        let connection = rusqlite::Connection::open(overlap_path.join("events.sqlite3"))
            .expect("overlap database");
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='events'",
                [],
                |row| row.get(0),
            )
            .expect("journal table count");
        assert_eq!(tables, 0);
    });
}

#[test]
fn session_journal_survives_reopen_and_rejects_invalid_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let state_path = temp.path().join("state");
        let hostile_title = "Continued\u{1b}[31m\u{202e} Session";
        let id = SessionId::new();
        let store = Store::open(StateRoot::admit(&state_path).expect("state admission"))
            .expect("new store");
        let start = store
            .append(
                id,
                Event::SessionStarted {
                    title: "First Session".into(),
                    workspace_identity: None,
                },
            )
            .await
            .expect("committed start");
        assert_eq!(start.sequence, 1);
        let rename = store
            .append(
                id,
                Event::SessionRenamed {
                    title: hostile_title.into(),
                },
            )
            .await
            .expect("committed rename");
        assert_eq!(rename.sequence, 2);
        assert!(matches!(
            store
                .append(
                    id,
                    Event::SessionStarted {
                        title: "Invalid duplicate".into(),
                        workspace_identity: None,
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store.load_session(id).await.expect("replayed Events");
        assert_eq!(events.len(), 2);
        let view = SessionView::replay(id, &events)
            .expect("valid reduction")
            .expect("Session exists");
        assert_eq!(view.title, hostile_title);
        assert_eq!(view.last_sequence, 2);
        assert_eq!(view.workspace_identity, None);
        store.close().await.expect("closed reopened store");

        let text = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "text", &id.to_string()])
            .bounded_output()
            .expect("show process");
        assert!(text.status.success());
        assert_eq!(text.stderr, b"");
        assert_eq!(
            String::from_utf8(text.stdout).expect("UTF-8 output"),
            format!(
                "Session: {id}\nTitle: Continued\\u{{001b}}[31m\\u{{202e}} Session\n\
                 1 SessionStarted First Session\n\
                 2 SessionRenamed Continued\\u{{001b}}[31m\\u{{202e}} Session\n"
            )
        );
        let jsonl = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &id.to_string()])
            .bounded_output()
            .expect("JSONL show process");
        assert!(jsonl.status.success());
        assert_eq!(jsonl.stderr, b"");
        assert!(jsonl.stdout.ends_with(b"\n"));
        assert!(!jsonl.stdout.contains(&0x1b));
        assert!(!String::from_utf8_lossy(&jsonl.stdout).contains('\u{202e}'));
        let lines: Vec<_> = jsonl
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("valid JSONL"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "SessionStarted");
        assert_eq!(lines[1]["payload"]["title"], hostile_title);
        assert_eq!(jsonl.stdout.iter().filter(|byte| **byte == b'\n').count(), 2);
        for (line, event) in lines.iter().zip(&events) {
            assert_eq!(line["sequence"], event.sequence);
            assert_eq!(line["session_id"], id.to_string());
            assert!(line["run_id"].is_null());
            assert!(line["agent_run_id"].is_null());
            assert_eq!(line["event_version"], 1);
            assert_eq!(line["created_at_ms"], event.created_at_ms);
        }

        let missing_path = temp.path().join("missing");
        let missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&missing_path)
            .args(["--output", "text", &id.to_string()])
            .bounded_output()
            .expect("missing-state show process");
        assert!(!missing.status.success());
        assert_eq!(missing.stdout, b"");
        assert_eq!(
            missing.stderr,
            b"error: state directory unavailable or unsafe\n"
        );
        assert!(!missing_path.exists());

        let invalid_argument = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .arg("--\u{1b}[31m")
            .bounded_output()
            .expect("invalid-argument process");
        assert!(!invalid_argument.status.success());
        assert_eq!(invalid_argument.stdout, b"");
        assert_eq!(
            invalid_argument.stderr,
            b"error: invalid arguments; run arany --help\n"
        );

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES (3, ?1, 'UnknownEvent', 1, '{}', 1)",
                [id.to_string()],
            )
            .expect("injected unsupported Event");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after injection");
        assert!(matches!(
            store.load_session(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store after rejected replay");

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET kind='SessionRenamed', payload=?1 WHERE sequence=3",
                [format!("{{\"title\":\"{}\"}}", "x".repeat(65_536))],
            )
            .expect("injected oversized Event");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after oversized Event");
        assert!(matches!(
            store.load_session(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store after size rejection");
    });
}

#[test]
fn exec_process_rejects_unselected_or_unavailable_provider_before_workspace_admission() {
    let temp = tempfile::tempdir().expect("private temporary directory");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace directory");
    let state = temp.path().join("state");
    let missing_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-placeholder-key")
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "/literal"])
        .bounded_output()
        .expect("exec process");
    assert!(!missing_key.status.success());
    assert_eq!(missing_key.stdout, b"");
    assert_eq!(missing_key.stderr, b"error: OPENAI_API_KEY unavailable\n");
    assert!(!state.exists());

    #[cfg(unix)]
    for workspace_id in ["", "default", "wrkspc_a\r\nx-header:value"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ANTHROPIC_API_KEY", "test-placeholder-key")
            .env("ANTHROPIC_WORKSPACE_ID", workspace_id)
            .current_dir(temp.path())
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                "anthropic",
                "--model",
                "claude-opus-5-5",
                "prompt",
            ]);
        let rejected = process_output_before_deadline(command);
        assert!(!rejected.status.success());
        assert_eq!(rejected.stdout, b"");
        assert_eq!(rejected.stderr, b"error: invalid ANTHROPIC_WORKSPACE_ID\n");
        assert!(!state.exists(), "malformed API workspace created State");
    }

    let malformed_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "openai",
            "--model",
            "bad\nmodel",
            "--effort",
            "high",
            "prompt",
        ])
        .bounded_output()
        .expect("malformed native model process");
    assert!(!malformed_model.status.success());
    assert_eq!(malformed_model.stdout, b"");
    assert_eq!(malformed_model.stderr, b"error: invalid native model ID\n");
    assert!(!state.exists());

    for (profile, model, effort, expected_error) in [
        (
            "openai",
            "gpt-6.1-sol",
            "max",
            b"error: OPENAI_API_KEY unavailable\n".as_slice(),
        ),
        (
            "anthropic",
            "claude-opus-5-5",
            "medium",
            b"error: ANTHROPIC_API_KEY unavailable\n".as_slice(),
        ),
        (
            "openai",
            "future-model",
            "low",
            b"error: OPENAI_API_KEY unavailable\n".as_slice(),
        ),
        (
            "anthropic",
            "future-model",
            "low",
            b"error: ANTHROPIC_API_KEY unavailable\n".as_slice(),
        ),
    ] {
        let selection = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                profile,
                "--model",
                model,
                "--effort",
                effort,
                "prompt",
            ])
            .bounded_output()
            .expect("reviewed native model process");
        assert!(!selection.status.success(), "{profile}/{model}");
        assert_eq!(selection.stdout, b"", "{profile}/{model}");
        assert_eq!(selection.stderr, expected_error, "{profile}/{model}");
        assert!(!state.exists(), "{profile}/{model}");
    }

    let overlapping_state = workspace.join("state");
    let overlap = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&overlapping_state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "prompt"])
        .bounded_output()
        .expect("overlap process");
    assert!(!overlap.status.success());
    assert_eq!(overlap.stdout, b"");
    assert_eq!(
        overlap.stderr,
        b"error: state directory overlaps the Workspace\n"
    );
    assert!(!overlapping_state.exists());

    let unsupported_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "unsupported", "prompt"])
        .bounded_output()
        .expect("unsupported model process");
    assert!(!unsupported_model.status.success());
    assert_eq!(unsupported_model.stdout, b"");
    assert_eq!(
        unsupported_model.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let unsupported_effort = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--effort",
            "max",
            "prompt",
        ])
        .bounded_output()
        .expect("unsupported effort process");
    assert!(!unsupported_effort.status.success());
    assert_eq!(unsupported_effort.stdout, b"");
    assert_eq!(
        unsupported_effort.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let missing_anthropic_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("OPENAI_API_KEY", "test-placeholder-key")
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-5",
            "prompt",
        ])
        .bounded_output()
        .expect("Anthropic exec process");
    assert!(!missing_anthropic_key.status.success());
    assert_eq!(missing_anthropic_key.stdout, b"");
    assert_eq!(
        missing_anthropic_key.stderr,
        b"error: ANTHROPIC_API_KEY unavailable\n"
    );
    assert!(!state.exists());

    let unsupported_anthropic_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "anthropic",
            "--model",
            "unsupported",
            "prompt",
        ])
        .bounded_output()
        .expect("unsupported Anthropic process");
    assert!(!unsupported_anthropic_model.status.success());
    assert_eq!(unsupported_anthropic_model.stdout, b"");
    assert_eq!(
        unsupported_anthropic_model.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let no_provider = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--model", "gpt-5.4", "prompt"])
        .bounded_output()
        .expect("unselected provider process");
    assert!(!no_provider.status.success());
    assert_eq!(no_provider.stdout, b"");
    assert_eq!(
        no_provider.stderr,
        b"error: invalid arguments; run arany --help\n"
    );
    assert!(!state.exists());
}

#[cfg(unix)]
#[test]
fn private_state_admission_rejects_unsafe_path_objects() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp = tempfile::tempdir().expect("temporary root");
    let state = temp.path().join("state");
    StateRoot::admit(&state).expect("initial private state");

    let alias = temp.path().join("alias");
    symlink(&state, &alias).expect("test symlink");
    assert!(StateRoot::open_existing(&alias).is_err());

    let unnormalized = state.join("..").join("state");
    assert!(matches!(
        StateRoot::admit(&unnormalized),
        Err(StoreError::InvalidStateDirectory)
    ));

    let file = temp.path().join("ordinary-file");
    std::fs::write(&file, b"data").expect("test file");
    assert!(StateRoot::admit(&file.join("state")).is_err());

    let database = state.join("events.sqlite3");
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644))
        .expect("weak test permissions");
    assert!(matches!(
        StateRoot::open_existing(&state),
        Err(StoreError::StateNotPrivate)
    ));
}

#[test]
fn incomplete_run_replays_as_interrupted_and_a_new_run_can_start() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let identity_state = temp.path().join("identity");
        let identity_id = create_session(
            StateRoot::admit(&identity_state).unwrap(),
            workspace.clone(),
            None,
        )
        .await
        .unwrap();
        let identity = resume_session(
            StateRoot::open_existing(&identity_state).unwrap(),
            workspace.clone(),
            identity_id,
        )
        .await
        .unwrap()
        .workspace_identity
        .unwrap();
        for (prefix_len, cold, defaults_first) in [
            (2, false, false),
            (2, true, true),
            (5, false, true),
            (5, true, false),
            (7, false, false),
            (7, true, true),
        ] {
            let state_path = temp.path().join(format!("state-{prefix_len}-{cold}"));
            let session_id = SessionId::new();
            let interrupted_run = RunId::new();
            let primary = AgentRunId::new();
            let config = RunConfig {
                provider: "scripted".into(),
                model: "test-model".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                policy: CollaborationPolicy::Single,
                output_token_cap: 4096,
                provider_concurrency: 1,
                workspace_device: identity.0,
                workspace_inode: identity.1,
                instruction_digest: None,
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            };
            let mut store =
                Store::open(StateRoot::admit(&state_path).expect("private state")).expect("store");
            let prefix = [
                Event::SessionStarted {
                    title: "New Session".into(),
                    workspace_identity: None,
                },
                Event::MessageAccepted {
                    run_id: interrupted_run,
                    text: "Question".into(),
                    images: Vec::new(),
                },
                Event::RunStarted {
                    run_id: interrupted_run,
                    config: config.clone(),
                },
                Event::AgentSpawned {
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                },
                Event::ProviderCallRecorded {
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    record: arany::ProviderCallRecord {
                        phase: arany::AgentPhase::RootPlan,
                        disposition: arany::ProviderCallDisposition::Finished,
                        response_id: Some("synthetic-response".into()),
                        input_tokens: Some(7),
                        output_tokens: Some(3),
                        wire_provenance: None,
                        failure_reason: None,
                    },
                },
                Event::AgentFinished {
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    disposition: arany::AgentDisposition::Finished,
                    summary: Some("Observed summary".into()),
                    result: Some("Observed answer".into()),
                },
                Event::MessageCommitted {
                    run_id: interrupted_run,
                    text: "Observed answer".into(),
                },
            ];
            for event in &prefix[..prefix_len] {
                store
                    .append(session_id, event.clone())
                    .await
                    .expect("valid unfinished prefix");
            }
            if cold {
                store.close().await.expect("store shutdown");
                store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                    .expect("reopened store");
            }
            let events = store
                .load_session(session_id)
                .await
                .expect("durable prefix");
            let view = SessionView::replay(session_id, &events)
                .expect("valid prefix")
                .expect("Session");
            assert_eq!(view.runs[0].status, RunStatus::Interrupted);
            assert_eq!(
                view.workspace_identity,
                (prefix_len > 2).then_some(identity)
            );
            if prefix_len > 2 {
                let listed = list_sessions(
                    StateRoot::open_existing(&state_path).unwrap(),
                    workspace.clone(),
                )
                .await
                .unwrap();
                assert_eq!(
                    listed[0].title, "Question",
                    "unfinished primary summary must not supply an AI title"
                );
            }
            let interrupted = view.runs[0].clone();
            if prefix_len > 2 {
                assert_eq!(
                    interrupted.agents[0].status,
                    if prefix_len == 7 {
                        AgentStatus::Finished
                    } else {
                        AgentStatus::Interrupted
                    }
                );
                assert_eq!(interrupted.agents[0].provider_calls.len(), 1);
            }
            assert_eq!(
                interrupted.assistant_message.as_deref(),
                (prefix_len == 7).then_some("Observed answer")
            );
            assert!(interrupted.finished_sequence.is_none());
            let defaults = SessionDefaults {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                effort: Some(arany::Effort::High),
                account_id: None,
                policy: CollaborationPolicy::Single,
            };
            let mut metadata = [
                Event::SessionRenamed {
                    title: "Recovered conversation".into(),
                },
                Event::SessionDefaultChanged {
                    defaults: defaults.clone(),
                },
            ];
            if defaults_first {
                metadata.swap(0, 1);
            }
            for event in metadata {
                store
                    .append(session_id, event)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("idle metadata after prefix {prefix_len}, cold={cold}: {error}")
                    });
            }
            let committed = store
                .load_session(session_id)
                .await
                .expect("metadata prefix");
            assert_eq!(&committed[..prefix_len], events.as_slice());
            assert_eq!(committed.len(), prefix_len + 2);
            assert!(
                committed[prefix_len..]
                    .iter()
                    .all(|event| event.run_id.is_none() && event.agent_run_id.is_none())
            );
            let recovered = store
                .load_view(session_id)
                .await
                .expect("recovered view")
                .expect("Session");
            assert_eq!(recovered.title, "Recovered conversation");
            assert_eq!(recovered.defaults, defaults);
            assert_eq!(
                recovered.runs.as_slice(),
                std::slice::from_ref(&interrupted)
            );
            let late = match prefix_len {
                2 => Event::RunStarted {
                    run_id: interrupted_run,
                    config: config.clone(),
                },
                5 => prefix[5].clone(),
                7 => Event::RunFinished {
                    run_id: interrupted_run,
                    disposition: arany::RunDisposition::Finished,
                },
                _ => unreachable!("declared crash point"),
            };
            assert!(matches!(
                store.append(session_id, late).await,
                Err(StoreError::InvalidHistory)
            ));
            assert_eq!(
                store
                    .load_session(session_id)
                    .await
                    .expect("unchanged prefix"),
                committed
            );
            let next_run = RunId::new();
            store
                .append(
                    session_id,
                    Event::MessageAccepted {
                        run_id: next_run,
                        text: "Try again".into(),
                        images: Vec::new(),
                    },
                )
                .await
                .expect("new Run after interrupted prefix");
            store
                .append(
                    session_id,
                    Event::RunStarted {
                        run_id: next_run,
                        config,
                    },
                )
                .await
                .expect("fresh Run starts");
            store
                .append(
                    session_id,
                    Event::AgentSpawned {
                        run_id: next_run,
                        agent_run_id: AgentRunId::new(),
                        role: AgentRole::Primary,
                        ordinal: 0,
                        objective: None,
                    },
                )
                .await
                .expect("ordinary Run progress stays active");
            store.close().await.expect("store shutdown");
            let store = Store::open_read_only(
                StateRoot::open_existing(&state_path).expect("existing state"),
            )
            .expect("closed-history reopen");
            let events = store.load_session(session_id).await.expect("history");
            let view = SessionView::replay(session_id, &events)
                .expect("valid history")
                .expect("Session");
            assert_eq!(view.runs.len(), 2);
            assert_eq!(view.runs[0], interrupted);
            assert_eq!(view.runs[1].status, RunStatus::Interrupted);
            assert_eq!(view.defaults, defaults);
            assert_eq!(view.title, "Recovered conversation");
            store.close().await.expect("store shutdown");
        }
    });
}

#[cfg(unix)]
struct SupervisedChild(Option<std::process::Child>);

#[cfg(unix)]
impl SupervisedChild {
    fn child(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("live child")
    }

    fn finish(&mut self) -> std::process::Output {
        process::capture(
            self.child(),
            std::time::Duration::from_secs(5),
            32 * 1024 * 1024,
        )
    }
}

#[cfg(unix)]
impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn process_output_before_deadline(mut command: Command) -> std::process::Output {
    use std::process::Stdio;
    use std::time::Duration;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = SupervisedChild(Some(command.spawn().expect("product process")));
    process::capture(process.child(), Duration::from_secs(5), 32 * 1024 * 1024)
}

#[cfg(unix)]
struct HeldProvider {
    ready_path: std::path::PathBuf,
}

#[cfg(unix)]
impl arany::Provider for HeldProvider {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "held-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> impl std::future::Future<Output = Result<arany::ProviderResponse, arany::ProviderError>> + Send
    {
        let ready_path = self.ready_path.clone();
        async move {
            assert_eq!(
                request.phase,
                arany::AgentPhase::RootPlan,
                "holder call phase"
            );
            assert!(request.model == "held-model", "holder call model");
            assert!(request.images.is_empty(), "holder call images");
            assert!(
                request.objective == "Held in another process",
                "holder call objective"
            );
            assert!(request.instructions.is_none(), "holder call instructions");
            assert!(request.includes.is_empty(), "holder call includes");
            assert!(request.history.is_empty(), "holder call history");
            assert!(request.context_summary.is_none(), "holder call summary");
            assert!(request.child_results.is_empty(), "holder call children");
            assert_eq!(request.max_output_tokens, 4096, "holder call output cap");
            std::fs::write(ready_path, b"ready").expect("Provider readiness marker");
            std::future::pending().await
        }
    }
}

#[cfg(unix)]
#[test]
#[ignore = "supervised process helper"]
fn process_lock_holder() {
    use arany::{CollaborationPolicy, Engine, RunRequest};

    let state_path = std::env::var_os("ARANY_TEST_STATE_PATH").expect("state path");
    let workspace = std::env::var_os("ARANY_TEST_WORKSPACE_PATH").expect("Workspace path");
    let session_id = std::env::var("ARANY_TEST_SESSION_ID")
        .expect("Session ID")
        .parse::<SessionId>()
        .expect("valid Session ID");
    let ready_path = std::env::var_os("ARANY_TEST_READY_PATH").expect("ready path");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut engine = Engine::open(
            StateRoot::admit(std::path::Path::new(&state_path)).expect("state admission"),
            HeldProvider {
                ready_path: ready_path.into(),
            },
        )
        .expect("held Engine");
        engine
            .run(RunRequest {
                session_id: Some(session_id),
                title: None,
                objective: "Held in another process".into(),
                images: Vec::new(),
                workspace: workspace.into(),
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
            })
            .await
            .expect("Provider remains held");
    });
}

#[cfg(unix)]
#[test]
fn exec_process_rejects_a_busy_session_and_recovers_after_holder_death() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace directory");
        let state_path = temp.path().join("state");
        let session_id = SessionId::new();
        let store = Store::open(StateRoot::admit(&state_path).expect("state admission"))
            .expect("new store");
        store
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Locked Session".into(),
                    workspace_identity: None,
                },
            )
            .await
            .expect("committed Session");
        store.close().await.expect("store shutdown");

        let ready_path = temp.path().join("holder-ready");
        let mut holder = SupervisedChild(Some(
            Command::new(std::env::current_exe().expect("test executable"))
                .env_clear()
                .env("ARANY_TEST_STATE_PATH", &state_path)
                .env("ARANY_TEST_WORKSPACE_PATH", &workspace)
                .env("ARANY_TEST_SESSION_ID", session_id.to_string())
                .env("ARANY_TEST_READY_PATH", &ready_path)
                .args(["--ignored", "--exact", "process_lock_holder"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("lock holder process"),
        ));
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            assert!(
                holder.child().try_wait().expect("holder status").is_none(),
                "lock holder exited before readiness"
            );
            assert!(Instant::now() < ready_deadline, "lock holder deadline");
            std::thread::yield_now();
        }

        let contender = || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("OPENAI_API_KEY", "test-placeholder-key")
                .current_dir(temp.path())
                .args(["exec", "--state-dir"])
                .arg(&state_path)
                .arg("--workspace")
                .arg(&workspace)
                .args(["--provider", "openai", "--model", "gpt-5.4"])
                .args(["--session-id", &session_id.to_string()])
                .args(["--include", "missing.txt", "prompt"]);
            process_output_before_deadline(command)
        };
        let busy = contender();
        assert!(!busy.status.success());
        assert_eq!(busy.stdout, b"");
        assert_eq!(busy.stderr, b"error: Session has an active operation\n");

        holder.child().kill().expect("kill holder");
        let killed = holder.finish();
        assert!(!killed.status.success());
        let released = contender();
        assert!(!released.status.success());
        assert_eq!(released.stdout, b"");
        assert_eq!(released.stderr, b"error: explicit include does not exist\n");

        let store =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("read-only state"))
                .expect("read-only store");
        let events = store
            .load_session(session_id)
            .await
            .expect("unchanged Session");
        assert_eq!(events.len(), 4);
        let view = SessionView::replay(session_id, &events)
            .expect("valid interrupted prefix")
            .expect("Session");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert!(
            state_path
                .join(format!("session-{session_id}.lock"))
                .exists()
        );
        store.close().await.expect("read-only shutdown");
    });
}

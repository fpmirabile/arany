use super::process::BoundedOutput;
use arany::{StateRoot, Store, create_session};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

fn private_file(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("private storage fixture");
    file.write_all(bytes).expect("synthetic fixture bytes");
}

#[test]
fn storage_process_reports_missing_corrupt_and_unsafe_state_without_provider_or_mutation() {
    let temp = tempfile::tempdir().expect("storage process fixture");
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .expect("Workspace");
    let run = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(&workspace)
            .env("HOME", temp.path().join("home"))
            .env("XDG_STATE_HOME", temp.path().join("default-state"))
            .env("XDG_DATA_HOME", temp.path().join("default-data"))
            .args([
                "--otlp-endpoint",
                "invalid-ignored-for-local-storage",
                "storage",
                "--state-dir",
            ])
            .arg(&state);
        command.env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("accounts"));
        command.bounded_output().expect("bounded storage process")
    };
    let absent = run();
    assert!(absent.status.success());
    assert_eq!(absent.stderr, b"");
    assert!(String::from_utf8_lossy(&absent.stdout).contains("Selected StateRoot: absent"));
    assert!(!state.exists());
    assert!(!temp.path().join("accounts").exists());
    assert!(!String::from_utf8_lossy(&absent.stdout).contains("Output spill:"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fixture runtime");
    let id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("State"),
            workspace.clone(),
            None,
        ))
        .expect("fixture Session");
    let history = std::fs::read(state.join("events.sqlite3")).expect("fixture history");
    private_file(
        &state.join("private-secret-name"),
        b"PRIVATE_STORAGE_CANARY",
    );
    let without_spill = run();
    assert!(without_spill.status.success());
    assert!(!String::from_utf8_lossy(&without_spill.stdout).contains("Output spill:"));
    assert!(!state.join("spill").exists());
    let spill = state.join("spill");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&spill)
        .expect("spill directory");
    let output_run = spill.join("private-run-name");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&output_run)
        .expect("spill Run directory");
    let output_file = output_run.join("private-output-name.txt");
    private_file(&output_file, b"PRIVATE_SPILL_CANARY");
    private_file(&output_run.join("writer.lock"), b"");
    let spill_metadata = [
        &spill,
        &output_run,
        &output_file,
        &output_run.join("writer.lock"),
    ]
    .map(|path| std::fs::metadata(path).expect("spill metadata"));
    let logical = spill_metadata
        .iter()
        .map(std::fs::Metadata::len)
        .sum::<u64>();
    let allocated = spill_metadata
        .iter()
        .map(|metadata| {
            use std::os::unix::fs::MetadataExt;
            metadata.blocks() * 512
        })
        .sum::<u64>();
    let known = run();
    assert!(known.status.success());
    assert_eq!(known.stderr, b"");
    let text = String::from_utf8(known.stdout).expect("report text");
    assert!(text.starts_with("Arany storage: complete for covered locations\n"));
    assert!(text.contains("Known owned subtotal:"));
    assert!(!text.contains("private-secret-name"));
    assert!(!text.contains("PRIVATE_STORAGE_CANARY"));
    assert_eq!(text.matches("Output spill: complete\n").count(), 1);
    assert!(text.contains(&format!("Output spill: complete\n  allocated {allocated} B; logical {logical} B; 2 files; 2 directories\n")));
    assert!(!text.contains("private-run-name"));
    assert!(!text.contains("private-output-name.txt"));
    assert!(!text.contains("PRIVATE_SPILL_CANARY"));
    assert_eq!(
        std::fs::read(&output_file).expect("retained spill"),
        b"PRIVATE_SPILL_CANARY"
    );
    assert_eq!(
        std::fs::read_dir(&output_run)
            .expect("retained output and lock")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read(state.join("events.sqlite3")).expect("unchanged history"),
        history
    );
    runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
            .expect("reopen observation Store");
        let events = store.load_session(id).await.expect("reopened events");
        let view = store
            .load_view(id)
            .await
            .expect("reduced view")
            .expect("Session");
        assert_eq!(events.len(), 1);
        assert!(view.runs.is_empty());
        store.close().await.expect("close Store");
    });
    std::fs::write(
        state.join("events.sqlite3"),
        b"damaged history stays inspectable",
    )
    .expect("damaged history fixture");
    let corrupt = run();
    assert!(corrupt.status.success());
    assert_eq!(corrupt.stderr, b"");
    assert_eq!(
        std::fs::read(state.join("events.sqlite3")).expect("no repair"),
        b"damaged history stays inspectable"
    );
    private_file(&output_run.join("unsafe"), b"UNSAFE_STORAGE_CANARY");
    std::fs::set_permissions(
        output_run.join("unsafe"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("unsafe record");
    let partial = run();
    assert!(!partial.status.success());
    assert_eq!(
        partial.stderr,
        b"error: storage inventory partial; see known subtotal and coverage\n"
    );
    let text = String::from_utf8(partial.stdout).expect("partial report");
    assert!(text.starts_with("Arany storage: partial; known subtotal only\n"));
    assert!(!text.contains("UNSAFE_STORAGE_CANARY"));
    assert!(text.contains("Output spill: partial\n"));
    assert_eq!(
        std::fs::read(output_run.join("unsafe")).expect("unsafe record retained"),
        b"UNSAFE_STORAGE_CANARY"
    );
    assert!(!state.join("development.log").exists());
}

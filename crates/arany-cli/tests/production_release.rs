use arany::test_support::process::BoundedOutput;
use std::{path::Path, process::Command, time::Duration};

const DEADLINE: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: usize = 64 * 1024;

fn production_binary() -> std::path::PathBuf {
    let path =
        std::env::var_os("ARANY_PRODUCTION_EXE").expect("pinned ordinary release executable");
    let path = std::path::PathBuf::from(path);
    assert!(path.is_absolute() && path.is_file());
    path
}

fn storage_command(executable: &Path, root: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args(["storage", "--state-dir"])
        .arg(root.join("state"))
        .arg("--workspace")
        .arg(root.join("workspace"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("default-state"))
        .env("XDG_DATA_HOME", root.join("default-data"))
        .env("ARANY_TEST_ACCOUNT_ROOT", root.join("account"))
        .current_dir(root);
    command
}

#[test]
#[ignore = "requires a separately pinned ordinary release executable"]
fn ordinary_release_refuses_the_account_fixture_before_reading_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let fixture = storage_command(Path::new(env!("CARGO_BIN_EXE_arany")), &root)
        .bounded_output_for(DEADLINE, OUTPUT_LIMIT)
        .unwrap();
    assert!(fixture.status.success(), "fixture-enabled positive control");
    assert!(fixture.stderr.is_empty());
    assert!(String::from_utf8_lossy(&fixture.stdout).contains("Selected StateRoot: absent"));
    let production = storage_command(&production_binary(), &root)
        .bounded_output_for(DEADLINE, OUTPUT_LIMIT)
        .unwrap();
    assert_eq!(production.status.code(), Some(1));
    assert!(production.stdout.is_empty());
    assert_eq!(production.stderr, b"error: account location unavailable\n");
    for absent in ["state", "account", "default-state", "default-data", "home"] {
        assert!(!root.join(absent).exists(), "probe created {absent}");
    }
    #[cfg(target_os = "macos")]
    ordinary_release_refuses_the_keychain_fixture_before_store_access();
}

#[cfg(target_os = "macos")]
fn ordinary_release_refuses_the_keychain_fixture_before_store_access() {
    use security_framework::os::macos::keychain::{CreateOptions, SecKeychain};
    use std::os::unix::fs::PermissionsExt;

    let _gate = arany::test_support::process::NATIVE_GATE.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = arany::StateRoot::admit(&temp.path().join("keychain")).unwrap();
    let path = root.path().join("arany-test.keychain-db");
    let _interaction = SecKeychain::disable_user_interaction().unwrap();
    let _keychain = CreateOptions::new()
        .password("synthetic-production-probe-password")
        .prompt_user(false)
        .create(&path)
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    for (executable, expected) in [
        (std::path::PathBuf::from(env!("CARGO_BIN_EXE_arany")), 0),
        (production_binary(), 1),
    ] {
        let output = Command::new(executable)
            .args([
                "--internal-credential-helper",
                "probe",
                "credential-backend-probe",
            ])
            .env_clear()
            .env("ARANY_TEST_KEYCHAIN_ROOT", root.path())
            .current_dir(temp.path())
            .bounded_output_for(DEADLINE, OUTPUT_LIMIT)
            .unwrap();
        assert_eq!(output.status.code(), Some(expected));
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}

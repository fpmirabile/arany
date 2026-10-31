use arany::test_support::process::file_output;
use std::{fs, process::Command};

#[test]
fn generate_limits_matches_committed_reference() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(root.path())
        .arg("generate-limits");
    let output = file_output(&mut command, None);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        include_bytes!("../../../docs/specs/limits.md")
    );
}

#[test]
fn generate_schema_subcommand_writes_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(root.path())
        .arg("generate-schema");
    let stdout = file_output(&mut command, None);
    assert!(stdout.status.success(), "{stdout:?}");
    assert!(stdout.stderr.is_empty());
    assert_eq!(stdout.stdout, include_bytes!("fixtures/serve/schema.json"));
    let out = root.path().join("nested/export");
    command.arg("--out").arg(&out);
    for _ in 0..2 {
        let output = file_output(&mut command, None);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(
            fs::read(out.join("arany-protocol.schema.json")).unwrap(),
            stdout.stdout
        );
        assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    }
    let blocked = root.path().join("blocked");
    fs::write(&blocked, "existing").unwrap();
    let output = file_output(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(root.path())
            .arg("generate-schema")
            .arg("--out")
            .arg(&blocked),
        None,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: schema export failed: output unavailable or unsafe\n"
    );
    assert_eq!(fs::read(blocked).unwrap(), b"existing");
    fs::remove_file(out.join("arany-protocol.schema.json")).unwrap();
    let outside = root.path().join("outside.json");
    fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, out.join("arany-protocol.schema.json")).unwrap();
    let output = file_output(&mut command, None);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: schema export failed: output unavailable or unsafe\n"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    fs::remove_file(&outside).unwrap();
    let output = file_output(&mut command, None);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: schema export failed: output unavailable or unsafe\n"
    );
    assert!(!outside.exists());
    assert!(
        fs::symlink_metadata(out.join("arany-protocol.schema.json"))
            .unwrap()
            .is_symlink()
    );
    fs::remove_file(out.join("arany-protocol.schema.json")).unwrap();
    fs::create_dir(out.join("arany-protocol.schema.json")).unwrap();
    let output = file_output(&mut command, None);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: schema export failed: output unavailable or unsafe\n"
    );
    assert!(out.join("arany-protocol.schema.json").is_dir());
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
}

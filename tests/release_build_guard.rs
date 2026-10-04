#![cfg(unix)]

#[path = "common/process.rs"]
pub mod process;
use process::BoundedOutput;

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

#[test]
fn release_build_rejects_ambient_source_overrides_before_building() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-build.sh");
    let tools = tempfile::tempdir().expect("private tool directory");
    let rustc = tools.path().join("rustc");
    fs::write(&rustc, "#!/bin/sh\n: > \"$TOOL_MARKER\"\nexit 99\n").expect("write test executable");
    fs::set_permissions(&rustc, fs::Permissions::from_mode(0o700))
        .expect("make test executable runnable");
    let marker = tools.path().join("executed");
    for (name, value) in [
        ("AWS_LC_SYS_USE_SYSTEM", "0"),
        ("AWS_LC_SYS_SYSTEM_DIR", "/untrusted"),
        ("OPENSSL_DIR_X86_64_UNKNOWN_LINUX_GNU", "/untrusted"),
        ("LIBSQLITE3_SYS_USE_PKG_CONFIG", "1"),
        ("LIBSQLITE3_FLAGS", "-DSQLITE_UNREVIEWED"),
        ("SQLITE_MAX_VARIABLE_NUMBER", "1"),
        ("ICU4X_DATA_DIR", ""),
        ("LIBC_CI", ""),
        ("CC", "/unreviewed/compiler"),
        ("CC_x86_64-unknown-linux-gnu", "/unreviewed/compiler"),
        ("HOST_CC", "/unreviewed/compiler"),
        ("TARGET_CC", "/unreviewed/compiler"),
        ("CFLAGS", "-include /unreviewed.h"),
        ("CFLAGS_x86_64-unknown-linux-gnu", "-include /unreviewed.h"),
        ("HOST_CFLAGS", "-include /unreviewed.h"),
        ("TARGET_CFLAGS", "-include /unreviewed.h"),
    ] {
        let output = Command::new("bash")
            .arg(&script)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", tools.path().display()))
            .env("TOOL_MARKER", &marker)
            .env(name, value)
            .bounded_output()
            .expect("run release guard");
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(output.stdout.is_empty(), "{name}");
        assert!(
            !marker.exists(),
            "a build tool ran before refusal for {name}"
        );
        let stderr = String::from_utf8(output.stderr).expect("ASCII error");
        assert!(
            stderr.contains("ambient build override"),
            "{name}: {stderr}"
        );
        if !value.is_empty() {
            assert!(!stderr.contains(value), "override value leaked for {name}");
        }
    }
}

#[test]
fn release_build_rejects_path_resolved_emcc_before_running_tools() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-build.sh");
    let tools = tempfile::tempdir().expect("private tool directory");
    for name in ["emcc", "rustc"] {
        let executable = tools.path().join(name);
        fs::write(&executable, "#!/bin/sh\n: > \"$TOOL_MARKER\"\nexit 99\n")
            .expect("write test executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("make test executable runnable");
    }
    let marker = tools.path().join("executed");
    let output = Command::new("bash")
        .arg(&script)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", tools.path().display()))
        .env("TOOL_MARKER", &marker)
        .bounded_output()
        .expect("run release guard");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!marker.exists(), "a build tool ran before refusal");
    let stderr = String::from_utf8(output.stderr).expect("ASCII error");
    assert!(stderr.contains("emcc"), "{stderr}");
}

#[test]
fn release_bundle_rejects_arguments_before_building() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-bundle.sh");
    let output = Command::new("bash")
        .arg(&script)
        .arg("unexpected")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .bounded_output()
        .expect("run bundle entry point");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
}

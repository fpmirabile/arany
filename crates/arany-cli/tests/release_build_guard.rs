pub use arany::test_support::process;
use process::BoundedOutput;

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

#[test]
fn release_build_rejects_ambient_source_overrides_before_building() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release-build.sh");
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
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release-build.sh");
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
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release-bundle.sh");
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

#[test]
fn source_archive_contains_only_tracked_release_inputs() {
    let root = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/source-archive.sh");
    for dir in [
        "scripts",
        "crates/arany/src",
        "crates/arany/tests",
        "crates/arany-cli/src",
        "crates/arany-cli/tests",
        "tools",
    ] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    fs::copy(script, root.path().join("scripts/source-archive.sh")).unwrap();
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "clippy.toml",
        "LICENSE",
        "NOTICE",
        "crates/arany/Cargo.toml",
        "crates/arany/src/lib.rs",
        "crates/arany/tests/fixture.txt",
        "crates/arany-cli/Cargo.toml",
        "crates/arany-cli/src/main.rs",
        "crates/arany-cli/tests/fixture.txt",
    ] {
        fs::write(root.path().join(file), "tracked release fixture").unwrap();
    }
    fs::write(root.path().join(".gitignore"), "private-canary.txt\n").unwrap();
    fs::write(root.path().join("README.md"), "excluded root documentation").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec![
            "add",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "clippy.toml",
            "LICENSE",
            "NOTICE",
            "scripts",
            "crates",
            ".gitignore",
            "README.md",
        ],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .bounded_output()
            .unwrap();
        assert!(output.status.success(), "fixture Git failed");
    }
    for dir in [
        "scripts",
        "crates/arany/src",
        "crates/arany/tests",
        "crates/arany-cli/src",
        "crates/arany-cli/tests",
    ] {
        fs::write(
            root.path().join(dir).join("private-canary.txt"),
            "excluded private fixture",
        )
        .unwrap();
    }
    fs::write(root.path().join("scripts/scratch.txt"), "untracked fixture").unwrap();
    let cargo = root.path().join("tools/cargo");
    fs::write(&cargo, "#!/bin/sh\ncase \"$*\" in\n*crossterm*) echo 'crossterm v0.29.0' ;;\n*rustls-platform-verifier*) echo 'rustls-platform-verifier v0.7.1' ;;\n'check --offline --locked') test -f rust-toolchain.toml ;;\n*) exit 3 ;;\nesac\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new("bash")
        .arg(root.path().join("scripts/source-archive.sh"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("tools").display()),
        )
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "source archive fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let archive = stdout
        .trim()
        .strip_prefix("Verified source archive: ")
        .unwrap();
    let listed = Command::new("tar")
        .args(["-tzf", archive])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .bounded_output()
        .unwrap();
    assert!(listed.status.success());
    let members = String::from_utf8(listed.stdout).unwrap();
    for expected in [
        "arany-0.1.0/rust-toolchain.toml",
        "arany-0.1.0/clippy.toml",
        "arany-0.1.0/crates/arany/src/lib.rs",
        "arany-0.1.0/crates/arany-cli/src/main.rs",
        "arany-0.1.0/crates/arany/tests/fixture.txt",
        "arany-0.1.0/crates/arany-cli/tests/fixture.txt",
        "arany-0.1.0/scripts/source-archive.sh",
    ] {
        assert!(
            members.lines().any(|member| member == expected),
            "missing {expected}"
        );
    }
    assert!(!members.contains("private-canary.txt"));
    assert!(!members.contains("README.md"));
    assert!(!members.contains("scratch.txt"));
    assert!(!members.contains(".gitignore"));
}

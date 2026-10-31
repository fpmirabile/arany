use crate::tools::tests::{in_process_runtime, inspect_read};
use crate::tools::types::*;
use serde_json::{Value, json};

pub(super) fn glob(pattern: &str, path: &str, limit: u32) -> ToolCall {
    ToolCall::Glob {
        pattern: pattern.into(),
        path: path.into(),
        offset: 0,
        limit,
    }
}

pub(super) async fn glob_result(
    runtime: &super::ToolRuntime,
    call: ToolCall,
    result_bytes: u32,
) -> ToolObservation {
    let mut intent = runtime
        .intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call,
        )
        .unwrap();
    intent.limits.result_bytes = result_bytes;
    let observation = inspect_read(runtime, intent).await;
    assert!(observation.valid());
    observation
}

pub(super) fn glob_paths(observation: &ToolObservation) -> (Vec<String>, bool) {
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{}",
        observation.output
    );
    assert!(matches!(
        observation.attestation,
        Some(Attestation::InProcess { .. })
    ));
    let value: Value = serde_json::from_str(&observation.output).unwrap();
    let paths = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect();
    (paths, value["may_be_truncated"].as_bool().unwrap())
}

#[tokio::test]
async fn glob_returns_admitted_regular_files_in_path_order_with_honest_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for directory in ["src/deep", ".github", "target", "node_modules", "many"] {
        std::fs::create_dir_all(workspace.join(directory)).unwrap();
    }
    for (path, content) in [
        ("Cargo.toml", "[package]"),
        ("README.md", "readme"),
        (".gitignore", "target"),
        (".github/ci.yml", "on: push"),
        ("src/main.rs", "fn main() {}"),
        ("src/lib.rs", ""),
        ("src/deep/mod.rs", "mod deep;"),
        ("src/v1.txt", "v1"),
        ("src/v12.txt", "v12"),
        ("target/build.rs", "excluded"),
        ("node_modules/dep.rs", "excluded"),
    ] {
        std::fs::write(workspace.join(path), content).unwrap();
    }
    // Sizes come from metadata alone, beyond the 1 MiB content collector.
    std::fs::File::create(workspace.join("big.bin"))
        .unwrap()
        .set_len(2 * 1024 * 1024)
        .unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, expected, truncated) in [
        (
            glob("**/*.rs", ".", 100),
            &["src/deep/mod.rs", "src/lib.rs", "src/main.rs"][..],
            false,
        ),
        (
            glob("src/**", ".", 100),
            &[
                "src/deep/mod.rs",
                "src/lib.rs",
                "src/main.rs",
                "src/v1.txt",
                "src/v12.txt",
            ],
            false,
        ),
        (
            glob("*.{toml,md}", ".", 100),
            &["Cargo.toml", "README.md"],
            false,
        ),
        (glob("src/v?.txt", ".", 100), &["src/v1.txt"], false),
        (glob(".*", ".", 100), &[".gitignore"], false),
        (glob("**/*.yml", ".", 100), &[".github/ci.yml"], false),
        (
            glob("*.rs", "src", 100),
            &["src/lib.rs", "src/main.rs"],
            false,
        ),
        (
            glob("**/*.rs", ".", 2),
            &["src/deep/mod.rs", "src/lib.rs"],
            true,
        ),
        (glob("*.none", ".", 100), &[], false),
        (glob("target/*", ".", 100), &[], false),
    ] {
        let observation = glob_result(&runtime, call.clone(), 64 * 1024).await;
        assert_eq!(
            glob_paths(&observation),
            (
                expected.iter().map(|p| (*p).to_owned()).collect(),
                truncated
            ),
            "{call:?}"
        );
    }
    let big = glob_result(&runtime, glob("big.*", ".", 100), 1024).await;
    assert_eq!(
        serde_json::from_str::<Value>(&big.output).unwrap(),
        json!({"entries":[{"path":"big.bin","bytes":2 * 1024 * 1024}],"limit":100,"may_be_truncated":false,"next_offset":null})
    );

    let mut created = Vec::new();
    for (count, truncated) in [(99, false), (100, false), (101, true)] {
        // A fixed permutation keeps directory insertion order unrelated to path order.
        while created.len() < count {
            let name = format!("many/f{:03}.txt", (created.len() * 37) % 101);
            std::fs::write(workspace.join(&name), "x").unwrap();
            created.push(name);
        }
        let mut sorted = created.clone();
        sorted.sort();
        sorted.truncate(100);
        let observation = glob_result(&runtime, glob("many/*.txt", ".", 100), 64 * 1024).await;
        assert_eq!(
            glob_paths(&observation),
            (sorted.clone(), truncated),
            "{count}"
        );
        let small = glob_result(&runtime, glob("many/*.txt", ".", 100), 512).await;
        let (fitted, fitted_truncated) = glob_paths(&small);
        assert!(small.output.len() <= 512);
        assert!(fitted_truncated && !fitted.is_empty());
        assert_eq!(fitted[..], sorted[..fitted.len()]);
    }
    let long = format!("{}.txt", "a".repeat(160));
    std::fs::write(workspace.join("src").join(&long), "").unwrap();
    let unfit = glob_result(&runtime, glob("src/a*.txt", ".", 100), 128).await;
    assert_eq!(unfit.disposition, ToolDisposition::Limit);
    assert_eq!(unfit.limit_cause, Some(super::LimitCause::Resource));
}

#[tokio::test]
async fn glob_never_discloses_protected_or_unadmitted_entries_and_bounds_its_walk() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    for directory in ["src", ".git", ".ssh", "outside"] {
        std::fs::create_dir_all(workspace.join(directory)).unwrap();
    }
    std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
    for (path, content) in [
        ("src/main.rs", "fn main() {}"),
        ("src/hard_a.rs", "hard"),
        (".env", "TOKEN=glob-canary"),
        ("src/.env.local", "TOKEN=glob-canary"),
        ("src/.ENV", "TOKEN=glob-canary"),
        (".git/config.rs", "glob-canary"),
        (".ssh/id.rs", "glob-canary"),
        ("Cargo.toml", "[package]"),
    ] {
        std::fs::write(workspace.join(path), content).unwrap();
    }
    std::fs::hard_link(
        workspace.join("src/hard_a.rs"),
        workspace.join("src/hard_b.rs"),
    )
    .unwrap();
    std::fs::write(temp.path().join("elsewhere/escape.rs"), "glob-canary").unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("elsewhere/escape.rs"),
        workspace.join("src/link.rs"),
    )
    .unwrap();
    std::os::unix::fs::symlink(temp.path().join("elsewhere"), workspace.join("linked")).unwrap();
    let whole = in_process_runtime(
        &workspace,
        json!({"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for pattern in ["**", "**/*.rs", ".*", "**/.env*", "src/*"] {
        let observation = glob_result(&whole, glob(pattern, ".", 100), 64 * 1024).await;
        let (paths, _) = glob_paths(&observation);
        assert!(
            paths
                .iter()
                .all(|path| ["Cargo.toml", "src/main.rs"].contains(&path.as_str())),
            "{pattern}: {paths:?}"
        );
        for hidden in [
            "canary", ".env", ".ENV", ".git", ".ssh", "hard_", "link", "escape",
        ] {
            assert!(!observation.output.contains(hidden), "{pattern}: {hidden}");
        }
    }
    let selected = in_process_runtime(
        &workspace,
        json!({"workspace_paths":["Cargo.toml","src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, expected) in [
        (glob("**", ".", 100), &["Cargo.toml", "src/main.rs"][..]),
        (glob("*.rs", "src", 100), &["src/main.rs"]),
        (glob("*", ".", 100), &["Cargo.toml"]),
    ] {
        let observation = glob_result(&selected, call.clone(), 64 * 1024).await;
        assert_eq!(glob_paths(&observation).0, expected, "{call:?}");
    }
    let outside = glob_result(&selected, glob("**", "outside", 100), 1024).await;
    assert_eq!(outside.disposition, ToolDisposition::Denied);

    for (call, disposition, output) in [
        (
            glob("*", "Cargo.toml", 100),
            ToolDisposition::Failed,
            "Tool operation failed",
        ),
        (
            glob("*", "missing", 100),
            ToolDisposition::Failed,
            "File or directory not found",
        ),
        (
            glob("*", "linked", 100),
            ToolDisposition::Denied,
            "outside the admitted regular-file boundary",
        ),
    ] {
        let observation = glob_result(&whole, call.clone(), 1024).await;
        assert_eq!(observation.disposition, disposition, "{call:?}");
        assert!(
            observation.output.contains(output),
            "{}",
            observation.output
        );
        assert!(!observation.output.contains("canary"));
    }

    let deep = (0..16)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    std::fs::create_dir_all(workspace.join(&deep)).unwrap();
    std::fs::write(workspace.join(&deep).join("leaf.rs"), "").unwrap();
    let too_deep = glob_result(&whole, glob("**/*.rs", ".", 100), 1024).await;
    assert_eq!(too_deep.disposition, ToolDisposition::Limit);
    assert_eq!(too_deep.limit_cause, Some(super::LimitCause::Resource));
    assert!(too_deep.attestation.is_some());
    let shallow = glob_result(&whole, glob("*.toml", ".", 100), 1024).await;
    assert_eq!(glob_paths(&shallow).0, ["Cargo.toml"]);
    std::fs::remove_dir_all(workspace.join("d0")).unwrap();

    std::fs::create_dir_all(workspace.join("wide")).unwrap();
    for index in 0..=MAX_SNAPSHOT_FILES {
        std::fs::write(workspace.join(format!("wide/f{index:05}")), "").unwrap();
    }
    // More matches than one walk retains still return the first page with its continuation.
    let candidates = glob_result(&whole, glob("wide/*", ".", 100), 64 * 1024).await;
    let (paths, truncated) = glob_paths(&candidates);
    assert!(truncated);
    assert_eq!(paths.len(), 100);
    assert_eq!(paths[0], "wide/f00000");
    let page: Value = serde_json::from_str(&candidates.output).unwrap();
    assert_eq!(page["next_offset"], 100);
    let last = ToolCall::Glob {
        pattern: "wide/*".into(),
        path: ".".into(),
        offset: 2000,
        limit: 100,
    };
    let last = glob_result(&whole, last, 64 * 1024).await;
    let (paths, truncated) = glob_paths(&last);
    assert!(!truncated);
    assert_eq!(paths.len(), MAX_SNAPSHOT_FILES + 1 - 2000);
    assert_eq!(paths[0], "wide/f02000");
    assert_eq!(
        serde_json::from_str::<Value>(&last.output).unwrap()["next_offset"],
        Value::Null
    );
    let one = glob_result(&whole, glob("wide/f00000", ".", 100), 1024).await;
    assert_eq!(glob_paths(&one).0, ["wide/f00000"]);
}

#[tokio::test]
// Skipped in the Guard: local test IPC capability is missing.
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn glob_never_discloses_unix_socket_entries() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("visible.rs"), "fn main() {}").unwrap();
    let _socket =
        std::os::unix::net::UnixListener::bind(temp.path().join("socket-canary.rs")).unwrap();
    let runtime = in_process_runtime(
        temp.path(),
        json!({"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for pattern in ["**", "**/*.rs", "*"] {
        let observation = glob_result(&runtime, glob(pattern, ".", 100), 1024).await;
        assert_eq!(glob_paths(&observation), (vec!["visible.rs".into()], false));
        assert!(!observation.output.contains("socket-canary"));
    }
}

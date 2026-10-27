use crate::tools::glob_tests::glob;
use crate::tools::search_tests::search;
use crate::tools::tests::{in_process_runtime, inspect_read, run_in_process};
use crate::tools::types::*;
use crate::validate::digest;
use serde_json::{Value, json};

#[tokio::test]
async fn listing_without_a_fitting_row_is_a_limit_and_empty_results_succeed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let long_name = format!("{}.txt", "a".repeat(160));
    std::fs::write(workspace.join("src").join(&long_name), "quoted").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for (call, result_bytes) in [
        (
            ToolCall::Search {
                path: "src".into(),
                query: "quoted".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            128,
        ),
        (ToolCall::List { path: "src".into() }, 128),
    ] {
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = result_bytes;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(
            observation.disposition,
            ToolDisposition::Limit,
            "{call:?} with one unfit row"
        );
    }
    for call in [
        ToolCall::Search {
            path: "src".into(),
            query: "absent".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::List {
            path: "empty".into(),
        },
    ] {
        std::fs::create_dir_all(workspace.join("empty")).unwrap();
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            call.clone(),
        );
        intent.limits.result_bytes = 1024;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{call:?} with no matches"
        );
        let rows: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(rows["entries"], json!([]));
    }
}

fn ignored_discovery_fixture(roots: Value) -> (tempfile::TempDir, super::ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    std::fs::create_dir_all(workspace.join("notes/pruned")).unwrap();
    std::fs::write(
        workspace.join("notes/.gitignore"),
        b"*.cache\npruned/\n!.env\n!.env.*\n!.aws/\n!.ssh/\n!.gnupg/\n!.git/\n!.npmrc\n!.pypirc\n",
    )
    .unwrap();
    std::fs::write(workspace.join("notes/visible.txt"), b"hit visible\n").unwrap();
    std::fs::write(
        workspace.join("notes/ignored.cache"),
        b"hit IGNORED_PAYLOAD_CANARY\n",
    )
    .unwrap();
    std::fs::write(workspace.join("notes/pruned/.gitignore"), b"!leaf\n").unwrap();
    std::fs::File::create(workspace.join("notes/pruned/leaf"))
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    std::fs::File::create(workspace.join("notes/oversized.cache"))
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    for path in [
        ".ENV",
        ".Env.local",
        ".AWS/credentials",
        ".SSH/key",
        ".GNUPG/key",
        ".GIT/config",
        ".NPMRC",
        ".PYPIRC",
    ] {
        let path = workspace.join("notes").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"hit PROTECTED_PAYLOAD_CANARY\n").unwrap();
    }
    let runtime = in_process_runtime(
        workspace,
        json!({"version":1,"workspace_paths":roots,"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    (temp, runtime)
}

#[tokio::test]
async fn typed_discovery_prunes_ignored_payloads_and_preserves_explicit_access() {
    for roots in [json!(["."]), json!(["notes"])] {
        let (_temp, runtime) = ignored_discovery_fixture(roots);
        for call in [
            ToolCall::List {
                path: "notes".into(),
            },
            glob("**", "notes", 100),
            search("notes", "hit"),
            search("notes/pruned", "hit"),
        ] {
            let pruned_search =
                matches!(&call, ToolCall::Search { path, .. } if path == "notes/pruned");
            let result = run_in_process(&runtime, call).await;
            assert_eq!(result.disposition, ToolDisposition::Succeeded);
            if pruned_search {
                assert_eq!(
                    serde_json::from_str::<Value>(&result.output).unwrap()["entries"],
                    json!([])
                );
            } else {
                assert!(result.output.contains("visible.txt"));
            }
            for hidden in [
                "ignored.cache",
                "oversized.cache",
                "pruned/leaf",
                ".ENV",
                ".AWS",
                "PAYLOAD_CANARY",
            ] {
                assert!(!result.output.contains(hidden));
            }
        }
        for call in [
            ToolCall::Read {
                path: "notes/ignored.cache".into(),
                offset: None,
                limit: None,
            },
            search("notes/ignored.cache", "hit"),
        ] {
            let result = run_in_process(&runtime, call).await;
            assert_eq!(result.disposition, ToolDisposition::Succeeded);
            assert!(result.output.contains("IGNORED_PAYLOAD_CANARY"));
            assert!(matches!(
                result.attestation,
                Some(Attestation::InProcess { .. })
            ));
        }
    }
}

fn invalid_discovery_control(case: &str) -> (tempfile::TempDir, super::ToolRuntime) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    std::fs::create_dir(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/visible.txt"), b"hit visible\n").unwrap();
    let control = workspace.join(".gitignore");
    match case {
        "encoding" => std::fs::write(&control, [0xff]).unwrap(),
        "bytes" => {
            std::fs::write(&control, vec![b'#'; super::selection::CONTROL_BYTES + 1]).unwrap()
        }
        "expansion" => std::fs::write(&control, "*".repeat(17)).unwrap(),
        "symlink" => std::os::unix::fs::symlink("notes/visible.txt", &control).unwrap(),
        "hardlink" => {
            std::fs::write(workspace.join("notes/control-source"), b"ignored").unwrap();
            std::fs::hard_link(workspace.join("notes/control-source"), &control).unwrap()
        }
        "permission" | "grant" => std::fs::write(&control, b"ignored").unwrap(),
        _ => unreachable!("closed corpus case"),
    }
    let roots = if case == "grant" {
        json!(["notes"])
    } else {
        json!(["."])
    };
    let mut runtime = in_process_runtime(
        workspace,
        json!({"version":1,"workspace_paths":roots,"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    if case == "permission" {
        let bytes = br#"{"permissions":{"deny":["Read(./.gitignore)"]}}"#;
        runtime.permissions = Some(super::PermissionSnapshot {
            config: std::sync::Arc::new(
                super::permissions::PermissionConfig::parse(bytes).unwrap(),
            ),
            digest: digest(bytes),
        });
    }
    (temp, runtime)
}

#[tokio::test]
async fn discovery_control_failures_discard_results_and_do_not_taint_exact_reads() {
    for case in [
        "encoding",
        "bytes",
        "expansion",
        "symlink",
        "hardlink",
        "permission",
        "grant",
    ] {
        let (_temp, runtime) = invalid_discovery_control(case);
        for call in [
            ToolCall::List {
                path: "notes".into(),
            },
            glob("**", "notes", 100),
            search("notes", "hit"),
        ] {
            let result = run_in_process(&runtime, call).await;
            assert_ne!(result.disposition, ToolDisposition::Succeeded, "{case}");
            assert!(!result.output.contains("visible.txt") && !result.output.contains("ignored"));
        }
        let result = run_in_process(&runtime, search("notes/visible.txt", "hit")).await;
        assert_eq!(result.disposition, ToolDisposition::Succeeded, "{case}");
    }
}

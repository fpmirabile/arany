use crate::session::WorkspaceIdentity;
use crate::tools::tests::{in_process_runtime, inspect_read, run_in_process};
use crate::tools::types::*;
use serde_json::{Value, json};

#[tokio::test]
async fn in_process_reads_attest_and_refuse_secrets_links_and_a_replaced_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(workspace.join("src/.env"), "TOKEN=secret-canary\n").unwrap();
    std::fs::write(temp.path().join("outside.txt"), "outside-canary").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let read = |path: &str| ToolCall::Read {
        path: path.into(),
        offset: None,
        limit: None,
    };

    let observation = run_in_process(&runtime, read("src/main.rs")).await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(observation.valid());
    assert!(matches!(
        observation.attestation,
        Some(Attestation::InProcess { workspace_device, workspace_inode, .. })
            if WorkspaceIdentity { device: workspace_device, inode: workspace_inode } == runtime.workspace_identity
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&observation.output).unwrap()["text"],
        "1\tfn main() {}\n"
    );
    assert_eq!(
        runtime.reservation_overhead(&read("src/main.rs")),
        std::time::Duration::ZERO
    );
    assert_eq!(
        runtime.reservation_overhead(&ToolCall::Mkdir {
            path: "src/new".into()
        }),
        std::time::Duration::ZERO
    );
    assert!(
        runtime.reservation_overhead(&ToolCall::GitStatus { path: "src".into() })
            > std::time::Duration::ZERO
    );

    assert_eq!(
        runtime.reservation_overhead(&ToolCall::Command {
            program: "shell".into(),
            args: vec![],
            cwd: "".into(),
        }),
        super::guard::RESERVATION_OVERHEAD + super::selection::PHASE_TIME,
    );

    for pin in ["grant", "enforcement"] {
        let intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            read("src/main.rs"),
        );
        let presented = intent.clone();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        tokio::time::resume();
        let mut changed = runtime.clone();
        if pin == "grant" {
            changed.config.write = true;
        } else {
            changed.enforcement_digest = [9; 32];
        }
        let refused = changed.execute(intent, super::Cancellation::new()).await;
        assert_eq!(
            refused.intent, presented,
            "original approval pins stay fixed"
        );
        assert_eq!(refused.disposition, ToolDisposition::Failed, "{pin}");
        assert!(refused.attestation.is_none(), "{pin}");
        assert!(!refused.output.contains("fn main"), "{pin}");
    }

    let secret = run_in_process(&runtime, read("src/.env")).await;
    assert_eq!(secret.disposition, ToolDisposition::Denied);
    assert!(secret.attestation.is_none());
    for listing in [
        ToolCall::List { path: "src".into() },
        ToolCall::Search {
            path: "src".into(),
            query: "canary".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
    ] {
        let observation = run_in_process(&runtime, listing).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(
            !observation.output.contains(".env"),
            "{}",
            observation.output
        );
        assert!(
            !observation.output.contains("canary"),
            "{}",
            observation.output
        );
    }

    std::os::unix::fs::symlink(
        temp.path().join("outside.txt"),
        workspace.join("src/escape"),
    )
    .unwrap();
    let escape = run_in_process(&runtime, read("src/escape")).await;
    assert_eq!(escape.disposition, ToolDisposition::Denied);
    assert!(!escape.output.contains("outside-canary"));
    assert!(escape.valid());

    let mut invalid_intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        read("src/main.rs"),
    );
    let control = runtime
        .execute(invalid_intent.clone(), super::Cancellation::new())
        .await;
    assert_eq!(control.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        control.attestation,
        Some(Attestation::InProcess { .. })
    ));
    invalid_intent.use_count = 2;
    let invalid = runtime
        .execute(invalid_intent, super::Cancellation::new())
        .await;
    assert_eq!(invalid.disposition, ToolDisposition::Failed);
    assert!(invalid.attestation.is_none());

    std::fs::rename(&workspace, temp.path().join("moved")).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "replacement").unwrap();
    let replaced = run_in_process(&runtime, read("src/main.rs")).await;
    assert_eq!(replaced.disposition, ToolDisposition::Failed);
    assert!(replaced.attestation.is_none());
    assert!(replaced.stops_run());
    assert!(
        replaced.output.contains("pinned input"),
        "{}",
        replaced.output
    );
    assert!(!replaced.output.contains("replacement"));
}

#[tokio::test]
async fn in_process_results_fit_their_budget_before_serialization() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let text = "\"quoted\\line\"\n".repeat(16 * 1024);
    std::fs::write(workspace.join("src/big.txt"), &text).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    for result_bytes in [256, 4096, MAX_TOOL_RESULT_BYTES as u32] {
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            crate::session::AgentRunId::new(),
            ToolCall::Read {
                path: "src/big.txt".into(),
                offset: None,
                limit: None,
            },
        );
        intent.limits.result_bytes = result_bytes;
        let observation = inspect_read(&runtime, intent).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.output.len() <= result_bytes as usize);
        let page: Value = serde_json::from_str(&observation.output).unwrap();
        let next = page["next_line"].as_u64().unwrap() as usize;
        assert!(next > 0 && next < text.len());
        assert_eq!(page["truncated"], true);
        assert_eq!(
            page["text"],
            (1..next)
                .map(|n| format!("{n}\t\"quoted\\line\"\n"))
                .collect::<String>()
        );
    }
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Search {
            path: "src".into(),
            query: "quoted".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
    );
    intent.limits.result_bytes = 1024;
    let observation = inspect_read(&runtime, intent).await;
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(observation.output.len() <= 1024);
    let rows: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(rows["may_be_truncated"], true);
    assert!(!rows["entries"].as_array().unwrap().is_empty());
    let mut intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        ToolCall::Read {
            path: "src/big.txt".into(),
            offset: None,
            limit: Some(1),
        },
    );
    intent.limits.result_bytes = 128;
    let observation = inspect_read(&runtime, intent).await;
    assert_eq!(observation.disposition, ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(super::LimitCause::Resource));
}

#[test]
fn the_primary_prompt_states_the_read_page_limit() {
    assert!(
        include_str!("../../provider/prompts/primary.txt").contains(&format!(
            "a nullable line limit from 1 to {MAX_READ_PAGE_LINES},"
        ))
    );
}

#[tokio::test]
async fn in_process_repeated_reads_all_succeed_on_a_large_file() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n".repeat(4096)).unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let mut elapsed = [Duration::ZERO; 40];
    for sample in &mut elapsed {
        let start = Instant::now();
        let observation = run_in_process(
            &runtime,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: None,
            },
        )
        .await;
        *sample = start.elapsed();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 2000);
        assert_eq!(result["next_line"], 2001);
        assert_eq!(result["text"].as_str().unwrap().lines().count(), 2000);
    }
    elapsed.sort_unstable();
    eprintln!(
        "Read latency: 40 sequential reads of a freshly written 53248-byte file; p50={:?}, p95={:?}, max={:?}",
        elapsed[19], elapsed[37], elapsed[39]
    );
}

#[tokio::test]
async fn in_process_reads_are_refused_before_dispatch_when_every_slot_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    let runtime = in_process_runtime(
        &workspace,
        json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let slots = std::sync::Arc::clone(&runtime.reads.as_ref().unwrap().slots);
    let held = slots
        .try_acquire_many_owned(super::MAX_IN_PROCESS_READS as u32)
        .unwrap();
    let refused = run_in_process(
        &runtime,
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(16),
        },
    )
    .await;
    drop(held);
    assert_eq!(refused.disposition, ToolDisposition::Limit);
    assert_eq!(refused.limit_cause, Some(super::LimitCause::ReadCapacity));
    assert!(refused.attestation.is_none());
    assert!(!refused.stops_run());
    assert!(!refused.output.contains("fn main"));
}

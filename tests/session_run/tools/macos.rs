use crate::process::BoundedOutput;
use arany::{
    CollaborationPolicy, CompactionRequest, CompactionResponse, Engine, Finish, Provider,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus,
    SessionView, StateRoot, Store, ToolCall, ToolDisposition, ToolResourceProfile,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

struct Journey {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    calls: Vec<ToolCall>,
}

impl Provider for Journey {
    fn profile_name(&self) -> &str {
        "scripted"
    }
    fn model_name(&self) -> &str {
        "file-tool-model"
    }
    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let step = requests.len();
        let context = request
            .tools
            .as_ref()
            .ok_or(ProviderError::InvalidOutcome)?;
        let catalog: serde_json::Value = serde_json::from_str(&context.catalog).unwrap();
        assert_eq!(catalog["commands"], json!([]));
        assert_eq!(catalog["mcp_servers"], json!([]));
        assert_eq!(context.observations.len(), step);
        let outcome = self.calls.get(step).cloned().map_or_else(
            || {
                ProviderOutcome::Finish(Finish {
                    summary: "Completed native file journey".into(),
                    result: "Created and edited synthetic files.".into(),
                })
            },
            ProviderOutcome::Tool,
        );
        requests.push(request);
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: None,
            output_tokens: None,
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[test]
fn guarded_native_files_and_skills_survive_closed_replay() {
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::native_file_journey_helper",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env("ARANY_TEST_FILE_JOURNEY_ROOT", temp.path())
        .current_dir(temp.path())
        .bounded_output_for(Duration::from_secs(60), 32 * 1024)
        .unwrap();
    assert!(
        output.status.success(),
        "native file journey: {:?}; stdout={:?}; stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    assert!(output.stderr.is_empty());
}

#[tokio::test]
#[ignore = "isolated helper owned by native file journey"]
async fn native_file_journey_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/main.txt"), b"before\n");
    private_file(&workspace.join("omitted.txt"), b"OMITTED_NATIVE_CANARY");
    private_file(&root.join("outside.txt"), b"OUTSIDE_NATIVE_CANARY");
    std::os::unix::fs::symlink(root.join("outside.txt"), workspace.join("src/linked.txt")).unwrap();
    let skill = workspace.join(".agents/skills/synthetic");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    private_file(
        &skill.join("SKILL.md"),
        b"---\nname: synthetic\ndescription: Synthetic guidance\n---\nUse typed file tools.\n",
    );
    private_file(
        &skill.join("references/note.txt"),
        b"Pinned synthetic resource.\n",
    );
    let state_path = root.join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(&state_path.join("tools.json"), &serde_json::to_vec(&json!({
        "version":1,"workspace_paths":["src",".agents/skills"],"write":true,"commands":[],"skills":[],"mcp":[]
    })).unwrap());
    let calls = vec![
        ToolCall::Read {
            path: "src/main.txt".into(),
            offset: 0,
            limit: 4096,
        },
        ToolCall::Edit {
            path: "src/main.txt".into(),
            expected_digest: hash(b"before\n"),
            old: "before".into(),
            new: "after".into(),
        },
        ToolCall::Mkdir {
            path: "src/generated".into(),
        },
        ToolCall::Write {
            path: "src/generated/note.txt".into(),
            expected_digest: None,
            content: "created\n".into(),
        },
        ToolCall::Write {
            path: "src/generated/note.txt".into(),
            expected_digest: Some(hash(b"created\n")),
            content: "replaced\n".into(),
        },
        ToolCall::List {
            path: "src/generated".into(),
        },
        ToolCall::Read {
            path: "src/main.txt".into(),
            offset: 0,
            limit: 4096,
        },
        ToolCall::Search {
            path: "src/main.txt".into(),
            query: "after".into(),
        },
        ToolCall::Skill {
            name: "synthetic".into(),
            resource: None,
        },
        ToolCall::Skill {
            name: "synthetic".into(),
            resource: Some("references/note.txt".into()),
        },
        ToolCall::Write {
            path: "src/generated/note.txt".into(),
            expected_digest: None,
            content: "must not overwrite".into(),
        },
        ToolCall::Edit {
            path: "src/main.txt".into(),
            expected_digest: hash(b"before\n"),
            old: "after".into(),
            new: "must not apply".into(),
        },
        ToolCall::Read {
            path: "src/linked.txt".into(),
            offset: 0,
            limit: 4096,
        },
    ];
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut engine = Engine::open(
        state,
        Journey {
            requests: requests.clone(),
            calls: calls.clone(),
        },
    )
    .unwrap();
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .unwrap();
    let outcome = engine
        .run(RunRequest {
            session_id: None,
            title: Some("Native files".into()),
            objective: "Complete the synthetic native file journey".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
        })
        .await
        .unwrap();
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "native observations: {:?}",
        outcome
            .run
            .tools
            .iter()
            .map(|tool| (
                &tool.intent.call,
                tool.observation
                    .as_ref()
                    .map(|observation| (&observation.disposition, &observation.output))
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(requests.lock().unwrap().len(), calls.len() + 1);
    assert_eq!(
        std::fs::read(workspace.join("src/main.txt")).unwrap(),
        b"after\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/generated/note.txt")).unwrap(),
        b"replaced\n"
    );
    let store = Store::open_read_only(StateRoot::admit(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    let run = replay.runs.last().unwrap();
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.tools.len(), calls.len());
    for (index, tool) in run.tools.iter().enumerate() {
        let observation = tool.observation.as_ref().expect("persisted receipt");
        assert_eq!(observation.intent.call, calls[index]);
        assert_eq!(
            observation.disposition,
            match index {
                10 | 11 => ToolDisposition::Conflict,
                12 => ToolDisposition::Denied,
                _ => ToolDisposition::Succeeded,
            }
        );
        assert_eq!(
            observation.intent.limits.resource_profile,
            ToolResourceProfile::MacosSupervised
        );
        let receipt = observation.guard.as_ref().unwrap();
        assert_eq!(receipt.contract_version, 1);
        assert_eq!(
            receipt.enforcement_digest,
            observation.intent.enforcement_digest
        );
        assert_eq!(receipt.limits, observation.intent.limits);
    }
    let observed = format!("{:?}", requests.lock().unwrap()).into_bytes();
    let journal = format!("{events:?}").into_bytes();
    for bytes in [&observed, &journal] {
        for canary in [
            b"OMITTED_NATIVE_CANARY".as_slice(),
            b"OUTSIDE_NATIVE_CANARY".as_slice(),
        ] {
            assert!(
                !bytes.windows(canary.len()).any(|part| part == canary),
                "unselected bytes escaped"
            );
        }
    }
    store.close().await.unwrap();
    guard_refuses_before_go(root, &workspace, &outcome.run.tools[0].intent);
}

fn guard_refuses_before_go(root: &Path, workspace: &Path, admitted: &arany::EffectIntent) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let config_bytes = br#"{"version":1,"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}"#;
    for (case, stage) in [
        ("eof", "handshake"),
        ("invalid_go", "handshake"),
        ("stale_workspace", "workspace"),
        ("resource_mismatch", "admission"),
    ] {
        let mut intent = admitted.clone();
        intent.id = uuid::Uuid::now_v7();
        intent.policy_digest = Sha256::digest(config_bytes).into();
        intent.call = ToolCall::Write {
            path: "src/never-dispatched.txt".into(),
            expected_digest: None,
            content: "must not be created".into(),
        };
        if case == "stale_workspace" {
            intent.workspace_inode += 1;
        }
        if case == "resource_mismatch" {
            intent.limits.resource_profile = ToolResourceProfile::LinuxKernel;
        }
        let task = serde_json::to_vec(
            &json!({"config":serde_json::from_slice::<serde_json::Value>(config_bytes).unwrap(),
            "intent":intent,"workspace":workspace,"guard_executable":env!("CARGO_BIN_EXE_arany")}),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_arany"))
            .arg("--internal-tool-guard")
            .env_clear()
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut owned = crate::loopback::ChildGuard::new(child);
        let mut stdin = owned.child().stdin.take().unwrap();
        stdin.write_all(&task).unwrap();
        stdin.write_all(b"\n").unwrap();
        if case == "invalid_go" {
            stdin.write_all(b"NO\n").unwrap();
        }
        drop(stdin);
        let output = crate::process::capture(owned.child(), Duration::from_secs(10), 4096);
        assert_eq!(
            output.status.code(),
            Some(1),
            "native Guard refusal: {case}"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stderr).unwrap(),
            json!({"stage":stage}),
            "native Guard refusal stage: {case}"
        );
        assert!(
            !workspace.join("src/never-dispatched.txt").exists(),
            "pre-GO effect: {case}"
        );
        if stage == "handshake" {
            let ready: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(ready["pid"].as_u64(), Some(u64::from(owned.child().id())));
            assert_eq!(ready["profile_digest"], json!(admitted.enforcement_digest));
        } else {
            assert!(output.stdout.is_empty(), "premature Ready: {case}");
        }
    }
}

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

const MCP_SCRIPT: &str = r#"set -eu
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"native-fixture","version":"1"}}}' ;;
        *'"method":"notifications/initialized"'*) ;;
        *'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}' ;;
        *'"method":"tools/call"'*) test "$(/bin/cat src/main.txt)" = after; /usr/bin/true; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"native MCP success"}]}}' ;;
        *) exit 1 ;;
    esac
done
"#;

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
    linked_file: Option<(std::path::PathBuf, std::path::PathBuf)>,
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
        assert_eq!(catalog["commands"][0]["name"], "shell");
        assert_eq!(catalog["mcp_servers"][0]["name"], "synthetic");
        assert_eq!(context.observations.len(), step);
        if let Some(ToolCall::Read { path, .. }) = self.calls.get(step)
            && path == "src/linked.txt"
            && let Some((target, link)) = &self.linked_file
        {
            std::os::unix::fs::symlink(target, link).unwrap();
        }
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
        .bounded_output_for(Duration::from_secs(180), 32 * 1024)
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
    let peer = root.join("mcp.sh");
    private_file(&peer, MCP_SCRIPT.as_bytes());
    let shell_digest = hash(&std::fs::read("/bin/sh").unwrap());
    let config = json!({
        "version":1,"workspace_paths":["src",".agents/skills"],"write":true,
        "commands":[{"name":"shell","executable":"/bin/sh","sha256":shell_digest,"interpreter":true,"inputs":[]}],
        "skills":[],
        "mcp":[{"name":"synthetic","program":{"name":"mcp-shell","executable":"/bin/sh","sha256":shell_digest,"interpreter":true,
            "inputs":[{"path":peer,"destination":"mcp.sh","sha256":hash(MCP_SCRIPT.as_bytes())}]},
            "args":["/inputs/mcp-shell/mcp.sh"],"tools":["echo"]}]
    });
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
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
        ToolCall::Command {
            program: "shell".into(),
            args: vec!["-c".into(), "set -eu; test \"$(/bin/cat src/main.txt)\" = after; test \"$(/bin/cat src/generated/note.txt)\" = replaced; /usr/bin/true; printf discarded > src/command-output.txt; printf native-command-success".into()],
            cwd: "".into(),
        },
        ToolCall::McpList {
            server: "synthetic".into(),
        },
        ToolCall::McpCall {
            server: "synthetic".into(),
            tool: "echo".into(),
            schema_digest: hash(&serde_json::to_vec(&json!({
                "name":"echo", "inputSchema":{"type":"object","properties":{},"additionalProperties":false}
            })).unwrap()),
            arguments: "{}".into(),
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
            linked_file: Some((root.join("outside.txt"), workspace.join("src/linked.txt"))),
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
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), calls.len() + 1);
        for request in requests.iter() {
            assert_eq!(request.phase, arany::AgentPhase::RootPlan);
            assert_eq!(request.model, "file-tool-model");
            assert_eq!(
                request.objective,
                "Complete the synthetic native file journey"
            );
            assert!(
                request.instructions.is_none()
                    && request.includes.is_empty()
                    && request.history.is_empty()
                    && request.context_summary.is_none()
                    && request.images.is_empty()
                    && request.child_results.is_empty()
            );
        }
    }
    assert_eq!(
        std::fs::read(workspace.join("src/main.txt")).unwrap(),
        b"after\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/generated/note.txt")).unwrap(),
        b"replaced\n"
    );
    assert!(!workspace.join("src/command-output.txt").exists());
    let store = Store::open_read_only(StateRoot::admit(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    let run = replay.runs.last().unwrap();
    assert_eq!(*run, outcome.run);
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.tools.len(), calls.len());
    for (index, tool) in run.tools.iter().enumerate() {
        let observation = tool.observation.as_ref().expect("persisted receipt");
        assert_eq!(observation.intent.call, calls[index]);
        assert_eq!(
            observation.disposition,
            match index {
                13 | 14 => ToolDisposition::Conflict,
                15 => ToolDisposition::Denied,
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
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &run.tools[10].observation.as_ref().unwrap().output
        )
        .unwrap(),
        json!({"exit_code":0,"stdout":"native-command-success","stderr":"","workspace_changes":"discarded"})
    );
    let mcp: serde_json::Value =
        serde_json::from_str(&run.tools[12].observation.as_ref().unwrap().output).unwrap();
    assert_eq!(mcp["content"][0]["text"], "native MCP success");
    let observed = format!("{:?}", requests.lock().unwrap()).into_bytes();
    let journal = format!("{events:?}").into_bytes();
    let database = std::fs::read(state_path.join("events.sqlite3")).unwrap();
    for bytes in [&observed, &journal, &database] {
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
    std::fs::remove_file(workspace.join("src/linked.txt")).unwrap();
    guard_refuses_before_go(root, &workspace, &outcome.run.tools[0].intent);
    stopped_effects_survive_closed_replay(root, &workspace, &config).await;
}

async fn stopped_effects_survive_closed_replay(
    root: &Path,
    workspace: &Path,
    config: &serde_json::Value,
) {
    let source = root.join("receipt-loss.c");
    let executable = root.join("receipt-loss");
    private_file(
        &source,
        br#"#include <stdlib.h>
#include <unistd.h>
int main(void) {
    const size_t bytes = 600UL * 1024 * 1024;
    volatile unsigned char *allocation = malloc(bytes);
    if (!allocation) return 2;
    for (size_t offset = 0; offset < bytes; offset += 4096) allocation[offset] = 1;
    for (;;) pause();
}
"#,
    );
    let compiled = std::process::Command::new("/usr/bin/xcrun")
        .args(["--sdk", "macosx", "clang", "-O0"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .env_clear()
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(20), 16 * 1024)
        .unwrap();
    assert!(
        compiled.status.success(),
        "native receipt fixture: {compiled:?}"
    );
    assert!(compiled.stdout.is_empty() && compiled.stderr.is_empty());
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    for case in [
        "denied",
        "cancelled",
        "overflow",
        "mcp-version",
        "receipt-loss",
    ] {
        let state_path = root.join(format!("state-{case}"));
        let state = StateRoot::admit(&state_path).unwrap();
        let mut config = config.clone();
        if case == "mcp-version" {
            let peer = root.join("mcp-unsupported.sh");
            let script = MCP_SCRIPT.replace("2025-11-25", "2024-11-05");
            private_file(&peer, script.as_bytes());
            config["mcp"][0]["program"]["inputs"][0]["path"] = json!(peer);
            config["mcp"][0]["program"]["inputs"][0]["sha256"] = json!(hash(script.as_bytes()));
        }
        if case == "receipt-loss" {
            config["commands"].as_array_mut().unwrap().push(json!({
                "name":"receipt-loss","executable":executable,
                "sha256":hash(&std::fs::read(&executable).unwrap()),"interpreter":false,"inputs":[]
            }));
        }
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let call = if case == "mcp-version" {
            ToolCall::McpList {
                server: "synthetic".into(),
            }
        } else if case == "receipt-loss" {
            ToolCall::Command {
                program: "receipt-loss".into(),
                args: Vec::new(),
                cwd: "".into(),
            }
        } else {
            let script = if case == "overflow" {
                "printf payload-started; i=0; while test $i -lt 32768; do printf x; i=$((i + 1)); done"
            } else {
                "printf must-not-dispatch > src/approval-output.txt"
            };
            ToolCall::Command {
                program: "shell".into(),
                args: vec!["-c".into(), script.into()],
                cwd: "".into(),
            }
        };
        let mut engine = Engine::open(
            state,
            Journey {
                requests: requests.clone(),
                calls: vec![call.clone()],
                linked_file: None,
            },
        )
        .unwrap();
        engine
            .enable_tools(env!("CARGO_BIN_EXE_arany").into())
            .unwrap();
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let attested = matches!(case, "overflow" | "mcp-version");
        if matches!(case, "denied" | "cancelled") {
            engine.set_tool_approvals(approvals);
        }
        let cancellation = arany::RunCancellation::new();
        let pending = cancellation.clone();
        let decision = async {
            if !matches!(case, "denied" | "cancelled") {
                return;
            }
            let action = inbox.next().await.expect("exact pending command");
            assert_eq!(action.intent.call, call);
            if case == "cancelled" {
                assert!(pending.cancel());
                drop(action);
            } else {
                action.decide(false);
            }
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                engine.run_with_progress(
                    RunRequest {
                        session_id: None,
                        title: Some(format!("Native {case}")),
                        objective: "Stop this synthetic native command".into(),
                        images: Vec::new(),
                        workspace: workspace.into(),
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                    },
                    cancellation,
                    arany::RunProgress::new()
                ),
                decision
            )
        })
        .await
        .expect("bounded stopped native command");
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        assert_eq!(
            requests.lock().unwrap().len(),
            if attested { 2 } else { 1 },
            "no automatic retry: {case}"
        );
        assert_eq!(
            outcome.run.status,
            if case == "cancelled" {
                RunStatus::Cancelled
            } else if attested {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            },
            "{case}: {:?}",
            outcome.run.tools
        );
        assert_eq!(outcome.run.assistant_message.is_some(), attested);
        assert!(!workspace.join("src/approval-output.txt").exists());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs[0], outcome.run, "closed replay: {case}");
        let observation = replay.runs[0].tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            match case {
                "denied" | "cancelled" => ToolDisposition::Denied,
                "overflow" => ToolDisposition::Limit,
                "mcp-version" => ToolDisposition::Failed,
                "receipt-loss" => ToolDisposition::Uncertain,
                _ => unreachable!(),
            },
            "{case}"
        );
        assert_eq!(observation.guard.is_some(), attested, "{case}");
    }
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
        intent.expires_at_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 60_000;
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

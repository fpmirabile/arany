use crate::process::BoundedOutput;
use crate::{hash, private_file};
use arany::{
    CollaborationPolicy, CompactionRequest, CompactionResponse, Engine, Finish, Provider,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus,
    SessionView, StateRoot, Store, ToolCall, ToolDisposition, ToolResourceProfile,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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

struct Journey {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    calls: Vec<ToolCall>,
    linked_file: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

const TRACE_STAGES: &[&str] = &[
    "tool_admission",
    "catalogue",
    "admission",
    "recovery",
    "connect",
    "controller_ready",
    "controller_receipt",
    "controller_coalition",
    "controller_bootout",
    "controller_storage",
    "supervisor_storage",
    "worker_ready",
    "worker_go",
    "worker_receipt",
    "worker_reap",
    "supervisor_coalition",
    "supervisor_detach",
];

fn trace_snapshot(root: &Path) -> serde_json::Value {
    use std::io::Read;
    let mut snapshot = serde_json::Map::new();
    for stage in std::iter::once("case").chain(TRACE_STAGES.iter().copied()) {
        let record = (|| {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(
                    root.join("account/guard-trace")
                        .join(format!("{stage}.json")),
                )
                .ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() > 256
            {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(257).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 256 {
                return None;
            }
            let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            if stage == "case" {
                let case = record["case"].as_str()?;
                if !matches!(
                    case,
                    "journey"
                        | "refusal"
                        | "denied"
                        | "cancelled"
                        | "overflow"
                        | "mcp-version"
                        | "receipt-loss"
                        | "runtime-mounts"
                        | "nonzero"
                        | "ai-approve"
                        | "deny"
                        | "drop"
                        | "cancel"
                        | "conflict"
                        | "scope"
                        | "partial"
                        | "symlink"
                        | "hardlink"
                        | "fifo"
                        | "setid"
                        | "excluded"
                        | "bytes-overflow"
                        | "files-overflow"
                        | "directory-conversion"
                        | "budget"
                ) {
                    return None;
                }
                return Some(json!({"case":case}));
            }
            let state = record["state"].as_str()?;
            if record["stage"] != stage || !matches!(state, "active" | "complete" | "interrupted") {
                return None;
            }
            let elapsed = record["elapsed_us"].as_u64();
            if (state == "active") != elapsed.is_none() {
                return None;
            }
            Some(json!({"state":state,"elapsed_us":elapsed}))
        })();
        snapshot.insert(stage.into(), record.unwrap_or(json!("unknown")));
    }
    json!({"stages":snapshot})
}

pub(super) fn trace_case(root: &Path, case: &str) {
    let trace = root.join("account/guard-trace");
    for stage in TRACE_STAGES {
        let path = trace.join(format!("{stage}.json"));
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("fixture trace reset: {error}"),
        }
    }
    private_file(
        &trace.join("case.json"),
        &serde_json::to_vec(&json!({"case":case})).unwrap(),
    );
}

pub(super) fn reset_tool_trace_stages(root: &Path) {
    let trace = root.join("account/guard-trace");
    for stage in TRACE_STAGES {
        if matches!(*stage, "tool_admission" | "catalogue") {
            continue;
        }
        let path = trace.join(format!("{stage}.json"));
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("fixture trace reset: {error}"),
        }
    }
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
        assert_eq!(
            context
                .observations
                .iter()
                .filter(|observation| !matches!(
                    observation.intent.call,
                    ToolCall::ApplyChangeset { .. }
                ))
                .count(),
            step
        );
        if let Some(ToolCall::Read { path, .. }) = self.calls.get(step)
            && path == "src/linked.txt"
            && let Some((target, link)) = &self.linked_file
        {
            std::os::unix::fs::symlink(target, link).unwrap();
        }
        if self.calls.get(step).is_some()
            && let Some(root) = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT")
        {
            reset_tool_trace_stages(Path::new(&root));
        }
        let outcome = self.calls.get(step).cloned().map_or_else(
            || {
                ProviderOutcome::Finish(Finish {
                    summary: "Completed native file journey".into(),
                    result: "Created and edited synthetic files.".into(),
                })
            },
            |call| {
                let plan_contract = (step == 0 && !matches!(call, ToolCall::McpList { .. }))
                    .then(|| {
                        let success_call = self
                            .calls
                            .iter()
                            .find(|candidate| {
                                matches!(
                                    candidate,
                                    ToolCall::Command { .. }
                                        | ToolCall::Write { .. }
                                        | ToolCall::Edit { .. }
                                        | ToolCall::Mkdir { .. }
                                        | ToolCall::Skill { .. }
                                        | ToolCall::McpCall { .. }
                                )
                            })
                            .cloned();
                        success_call.map(|success_call| {
                            let mut paths: Vec<_> = catalog["workspace_paths"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|path| path.as_str().map(str::to_owned))
                                .collect();
                            paths.sort();
                            paths.dedup();
                            let mut commands: Vec<_> = self
                                .calls
                                .iter()
                                .filter_map(|call| match call {
                                    ToolCall::Command { program, .. } => Some(program.clone()),
                                    _ => None,
                                })
                                .collect();
                            commands.sort();
                            commands.dedup();
                            let mut mcp_tools: Vec<_> = self
                                .calls
                                .iter()
                                .filter_map(|call| match call {
                                    ToolCall::McpCall { server, tool, .. } => {
                                        Some(arany::GoalMcpTool {
                                            server: server.clone(),
                                            tool: tool.clone(),
                                        })
                                    }
                                    _ => None,
                                })
                                .collect();
                            mcp_tools.sort_by(|left, right| {
                                (&left.server, &left.tool).cmp(&(&right.server, &right.tool))
                            });
                            mcp_tools.dedup();
                            let mut skills: Vec<_> = self
                                .calls
                                .iter()
                                .filter_map(|call| match call {
                                    ToolCall::Skill { name, resource } => {
                                        Some(arany::GoalSkillBinding {
                                            name: name.clone(),
                                            resource: if resource.as_deref() == Some("SKILL.md") {
                                                None
                                            } else {
                                                resource.clone()
                                            },
                                        })
                                    }
                                    _ => None,
                                })
                                .collect();
                            skills.sort_by(|left, right| {
                                (&left.name, left.resource.as_deref().unwrap_or("SKILL.md")).cmp(&(
                                    &right.name,
                                    right.resource.as_deref().unwrap_or("SKILL.md"),
                                ))
                            });
                            skills.dedup();
                            let success_criterion =
                                arany::GoalSuccessCriterion::ToolCallSucceeded {
                                    call: Box::new(success_call),
                                };
                            arany::GoalPlanContract {
                                mission: "Complete the bounded native Tool journey".into(),
                                paths,
                                commands,
                                resource_class: arany::ToolResourceClass::Default,
                                command_timeout_ms: None,
                                skills,
                                mcp_tools,
                                success_criterion,
                                budget: arany::GoalBudget::default(),
                            }
                        })
                    })
                    .flatten()
                    .map(Box::new);
                ProviderOutcome::Tool {
                    call,
                    plan_contract,
                }
            },
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
#[cfg_attr(
    not(arany_ci),
    ignore = "native launchd/Seatbelt Tool journey; about 35 seconds"
)]
fn guarded_native_files_and_skills_survive_closed_replay() {
    native_journey("tools::native_file_journey_helper");
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native command changeset integration and refusal corpus"
)]
fn guarded_command_changesets_survive_closed_replay() {
    native_journey("tools::native_command_changeset_helper");
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS selected-runtime Guard command and mounted-image refusal"
)]
fn guarded_runtime_profile_checks_mount_topology_before_dispatch() {
    native_journey("tools::native_runtime_mount_journey_helper");
}

fn native_journey(helper: &str) {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    for directory in [
        "account",
        "account/guard-trace",
        "home",
        "environment-state",
        "environment-data",
    ] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(temp.path().join(directory))
            .unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", helper, "--ignored", "--nocapture"])
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env(
            "ARANY_TEST_GUARD_TRACE",
            temp.path().join("account/guard-trace"),
        )
        .env("ARANY_TEST_FILE_JOURNEY_ROOT", temp.path())
        .env("HOME", temp.path().join("home"))
        .env("XDG_STATE_HOME", temp.path().join("environment-state"))
        .env("XDG_DATA_HOME", temp.path().join("environment-data"))
        .current_dir(temp.path());
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        command.bounded_output_for(Duration::from_secs(360), 32 * 1024)
    }))
    .unwrap_or_else(|_| {
        panic!(
            "native journey observer failed: {}",
            trace_snapshot(temp.path())
        )
    })
    .unwrap();
    assert!(
        output.status.success(),
        "native file journey: {:?}; stdout={:?}; stderr={:?}; trace={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        trace_snapshot(temp.path())
    );
    crate::process::assert_single_test_completed(&output.stdout, "native file journey");
    assert!(
        output.stderr.is_empty(),
        "native journey stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "isolated helper owned by native file journey"]
async fn native_file_journey_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    trace_case(root, "journey");
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
    private_file(&workspace.join("src/deleted.txt"), b"command delete\n");
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
            args: vec!["-c".into(), "set -eu; if /bin/cat /etc/passwd >/dev/null 2>&1; then exit 90; fi; if /bin/cat /usr/bin/sudo >/dev/null 2>&1; then exit 91; fi; test \"$(/bin/cat src/main.txt)\" = after; test \"$(/bin/cat src/generated/note.txt)\" = replaced; i=0; while test \"$i\" -lt 128; do /usr/bin/true; i=$((i + 1)); done; printf 'command create\\n' > src/command-output.txt; printf 'command modify\\n' > src/generated/note.txt; /usr/bin/find src/deleted.txt -type f -delete; printf native-command-success".into()],
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
            goal: arany::GoalRequest::default(),
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
        b"command modify\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/command-output.txt")).unwrap(),
        b"command create\n"
    );
    assert!(
        !workspace.join("src/deleted.txt").exists(),
        "delete publication: {:?}",
        &outcome.run.tools[9..11]
    );
    let store = Store::open_read_only(StateRoot::admit(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    let run = replay.runs.last().unwrap();
    assert_eq!(*run, outcome.run);
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.tools.len(), calls.len() + 1);
    let mut proposed = calls.iter();
    for (index, tool) in run.tools.iter().enumerate() {
        let observation = tool.observation.as_ref().expect("persisted receipt");
        if let ToolCall::ApplyChangeset { changeset, .. } = &observation.intent.call {
            assert_eq!(index, 10);
            assert_eq!(
                changeset
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>(),
                [
                    "src/command-output.txt",
                    "src/deleted.txt",
                    "src/generated/note.txt"
                ]
            );
        } else {
            assert_eq!(&observation.intent.call, proposed.next().unwrap());
        }
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
    let command: serde_json::Value =
        serde_json::from_str(&run.tools[9].observation.as_ref().unwrap().output).unwrap();
    assert_eq!(command["exit_code"], 0);
    assert_eq!(command["stdout"], "native-command-success");
    assert_eq!(command["stderr"], "");
    assert_eq!(command["changeset"]["files"].as_array().unwrap().len(), 3);
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
    let mut fresh = Engine::open(
        StateRoot::open_existing(&state_path).unwrap(),
        Journey {
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: vec![ToolCall::Command {
                program: "shell".into(),
                args: vec!["-c".into(), "set -eu; test \"$(/bin/cat src/command-output.txt)\" = 'command create'; test \"$(/bin/cat src/generated/note.txt)\" = 'command modify'; test ! -e src/deleted.txt; printf fresh-snapshot".into()],
                cwd: "".into(),
            }],
            linked_file: None,
        },
    ).unwrap();
    fresh
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .unwrap();
    let fresh_outcome = fresh
        .run(RunRequest {
            session_id: Some(outcome.session_id),
            title: None,
            objective: "Verify a fresh native command snapshot".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::default(),
        })
        .await
        .unwrap();
    fresh.close().await.unwrap();
    assert_eq!(fresh_outcome.run.status, RunStatus::Finished);
    let fresh_result: serde_json::Value = serde_json::from_str(
        &fresh_outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .output,
    )
    .unwrap();
    assert_eq!(fresh_result["stdout"], "fresh-snapshot");
    assert_eq!(fresh_result["changeset"]["files"], json!([]));
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let replay = store.load_view(outcome.session_id).await.unwrap().unwrap();
    assert_eq!(replay.runs.len(), 2);
    assert_eq!(replay.runs[1], fresh_outcome.run);
    store.close().await.unwrap();
    trace_case(root, "refusal");
    guard_refuses_before_go(root, &workspace, &outcome.run.tools[0].intent);
    stopped_effects_survive_closed_replay(root, &workspace, &config).await;
}

#[tokio::test]
#[ignore = "isolated helper owned by native command changeset corpus"]
async fn native_command_changeset_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
    let source = root.join("native-links.c");
    let executable = root.join("native-links");
    private_file(
        &source,
        br#"#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
static int write_file(const char *path, const void *bytes, size_t size) {
    FILE *file = fopen(path, "wb");
    return !file || fwrite(bytes, 1, size, file) != size || fclose(file);
}

int main(int argc, char **argv) {
    const unsigned char bytes[] = {0, 255};
    if (argc != 2 || write_file("src/a-created", bytes, 2) || chmod("src/a-created", 0700)
        || write_file("src/m-modified", "modified\n", 9) || unlink("src/z-deleted")) return 1;
    int result = !strcmp(argv[1], "symlink") ? symlink("m-modified", "src/hostile")
        : link("src/m-modified", "src/hostile");
    if (result) return 2;
    return fputs("payload-complete", stdout) == EOF ? 3 : 0;
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
        "native link fixture: {compiled:?}"
    );
    assert!(compiled.stdout.is_empty() && compiled.stderr.is_empty());
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = json!({
        "version":1,"workspace_paths":["src"],"write":true,
        "commands":[
            {"name":"shell","executable":"/bin/sh","sha256":hash(&std::fs::read("/bin/sh").unwrap()),"interpreter":true,"inputs":[]},
            {"name":"native-links","executable":executable,"sha256":hash(&std::fs::read(&executable).unwrap()),"interpreter":false,"inputs":[]}
        ],
        "skills":[],"mcp":[]
    });
    crate::tool_replay::native_command_changeset_corpus(root, &config, "shell").await;
}
struct RuntimeJourney {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    calls: Vec<ToolCall>,
    resource_class: arany::ToolResourceClass,
}

fn build_goal_request() -> arany::GoalRequest {
    let mut goal = arany::GoalRequest::new(
        arany::GoalBudget::default(),
        arany::GoalPhase::Implementation,
    );
    goal.resource_settings = arany::ToolResourceSettings {
        build_memory_bytes: 512 * 1024 * 1024,
        build_processes: 32,
        build_project_bytes: 128 * 1024 * 1024,
        build_cache_bytes: 64 * 1024 * 1024,
        ..arany::ToolResourceSettings::default()
    };
    goal
}

const RUNTIME_CACHE_CANARY: &str = "ARANY_RUNTIME_CACHE_PERSISTENCE_CANARY";

impl Provider for RuntimeJourney {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "runtime-profile-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = {
            let mut requests = self.requests.lock().unwrap();
            let step = requests.len();
            requests.push(request);
            step
        };
        if step > self.calls.len() {
            return Err(ProviderError::InvalidOutcome);
        }
        let outcome = if step < self.calls.len() {
            let call = self.calls[step].clone();
            if let Some(root) = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT") {
                reset_tool_trace_stages(Path::new(&root));
            }
            let plan_contract = if step == 0 {
                let program = match &call {
                    ToolCall::Command { program, .. } => program.clone(),
                    _ => return Err(ProviderError::InvalidOutcome),
                };
                let success_call = self
                    .calls
                    .last()
                    .cloned()
                    .ok_or(ProviderError::InvalidOutcome)?;
                Some(Box::new(arany::GoalPlanContract {
                    mission: "Run the selected runtime commands".into(),
                    paths: Vec::new(),
                    commands: vec![program],
                    resource_class: self.resource_class,
                    command_timeout_ms: None,
                    skills: Vec::new(),
                    mcp_tools: Vec::new(),
                    success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                        call: Box::new(success_call),
                    },
                    budget: arany::GoalBudget::default(),
                }))
            } else {
                None
            };
            ProviderOutcome::Tool {
                call,
                plan_contract,
            }
        } else {
            ProviderOutcome::Finish(Finish {
                summary: "Runtime profile checked".into(),
                result: "The selected runtime command completed under Guard, and its private build cache persisted between commands.".into(),
            })
        };
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

struct MountedRuntimeImage(std::path::PathBuf);

impl Drop for MountedRuntimeImage {
    fn drop(&mut self) {
        use crate::process::BoundedOutput;

        let _ = std::process::Command::new("/usr/bin/hdiutil")
            .args(["detach", "-force", "-quiet"])
            .arg(&self.0)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .bounded_output_for(Duration::from_secs(20), 4096);
    }
}

#[tokio::test]
#[ignore = "isolated helper owned by native macOS runtime profile journey"]
async fn native_runtime_mount_journey_helper() {
    use crate::process::BoundedOutput;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    trace_case(root, "runtime-mounts");
    assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
    let workspace = root.join("workspace");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .unwrap();
    let runtime = root.join("home/.nvm/versions/node/v22.3.0");
    std::fs::create_dir_all(runtime.join("bin")).unwrap();
    let node = runtime.join("bin/node");
    private_file(&node, b"#!/bin/sh\nprintf 'runtime-profile-selected\\n'\n");
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mountpoint = runtime.join("mounted-image");
    std::fs::create_dir(&mountpoint).unwrap();
    let image = root.join("runtime.sparseimage");
    let created = std::process::Command::new("/usr/bin/hdiutil")
        .args([
            "create",
            "-quiet",
            "-size",
            "8m",
            "-fs",
            "HFS+",
            "-volname",
            "AranyRuntimeFixture",
            "-type",
            "SPARSE",
            "-ov",
        ])
        .arg(&image)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(30), 4096)
        .unwrap();
    assert!(created.status.success(), "hdiutil create: {created:?}");
    let attached = std::process::Command::new("/usr/bin/hdiutil")
        .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
        .arg(&mountpoint)
        .arg(&image)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(30), 4096)
        .unwrap();
    assert!(attached.status.success(), "hdiutil attach: {attached:?}");
    let mounted = MountedRuntimeImage(mountpoint.clone());

    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    let options = permissions.runtime_profile_choices();
    let node = options
        .iter()
        .find(|option| option.label.starts_with("Node"))
        .expect("metadata-only Node candidate");
    let node_token = node.token.clone();
    let mut permissions = permissions;
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::AutoEdits,
            std::slice::from_ref(&node_token),
        )
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();

    let runtime_call = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; test \"$(node)\" = runtime-profile-selected; printf selected-runtime-command"
                .into(),
        ],
        cwd: String::new(),
    };
    let refused_requests = Arc::new(Mutex::new(Vec::new()));
    let refused_state = root.join("state-mounted");
    let mut engine = Engine::open(
        StateRoot::admit(&refused_state).unwrap(),
        RuntimeJourney {
            requests: refused_requests.clone(),
            calls: vec![runtime_call.clone()],
            resource_class: arany::ToolResourceClass::Default,
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    engine
        .enable_trusted_workspace(
            permissions.clone(),
            &workspace,
            env!("CARGO_BIN_EXE_arany").into(),
            approvals,
        )
        .unwrap();
    let run = engine.run(RunRequest {
        session_id: None,
        title: Some("Mounted runtime refusal".into()),
        objective: "Check runtime mount topology".into(),
        images: Vec::new(),
        workspace: workspace.clone(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::new(
            arany::GoalBudget::default(),
            arany::GoalPhase::Implementation,
        ),
    });
    let approve = async {
        let approval = inbox
            .next()
            .await
            .expect("one approval for the mounted-runtime command");
        assert_eq!(approval.intent.call, runtime_call);
        approval.decide(true);
    };
    let (refused, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(run, approve)
    })
    .await
    .expect("bounded mounted-runtime refusal journey");
    let refused = refused.unwrap();
    engine.close().await.unwrap();
    assert_eq!(
        refused.run.status,
        RunStatus::Failed,
        "runtime mount refusal Run failed: {}",
        crate::safe_run_diagnostics(&refused.run)
    );
    assert_eq!(refused.run.tools.len(), 1);
    assert_eq!(refused_requests.lock().unwrap().len(), 1);
    let refused_observation = refused.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(refused_observation.disposition, ToolDisposition::Failed);
    assert!(refused_observation.guard.is_none());
    assert!(
        refused_observation
            .output
            .contains("runtime mount topology")
    );
    assert_eq!(
        refused_requests.lock().unwrap()[0]
            .tools
            .as_ref()
            .unwrap()
            .observations
            .len(),
        0
    );
    let trace = trace_snapshot(root);
    assert_eq!(trace["stages"]["worker_go"], "unknown");
    let store = Store::open_read_only(StateRoot::open_existing(&refused_state).unwrap()).unwrap();
    let events = store.load_session(refused.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(refused.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.len(), 1);
    assert_eq!(replay.runs[0], refused.run, "closed runtime refusal replay");
    assert_eq!(replay.runs[0].status, RunStatus::Failed);
    let detached = std::process::Command::new("/usr/bin/hdiutil")
        .args(["detach", "-force", "-quiet"])
        .arg(&mountpoint)
        .env_clear()
        .current_dir(root)
        .bounded_output_for(Duration::from_secs(20), 4096)
        .unwrap();
    assert!(detached.status.success(), "hdiutil detach: {detached:?}");
    drop(mounted);

    let host_cache = root.join("home/.cargo/registry/cache");
    std::fs::create_dir_all(&host_cache).unwrap();
    private_file(&host_cache.join("canary.crate"), b"MACOS_HOST_CACHE_CANARY");
    let state_success = root.join("state-success");
    let cache_seed = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "if test -z \"${CARGO_HOME-}\"; then printf 'cache-seed:cargo-home-missing\\n' >&2; exit 90; fi; if ! mkdir -p \"$CARGO_HOME/registry/cache\" 2>/dev/null; then printf 'cache-seed:mkdir-failed\\n' >&2; exit 91; fi; set -eu; cache=\"$CARGO_HOME/registry/cache/private-bulk.crate\"; if ! { :; } 2>/dev/null > \"$cache\"; then printf 'cache-seed:create-failed\\n' >&2; exit 92; fi; i=0; while test \"$i\" -lt 40; do if ! { printf '%1048576s' ''; } 2>/dev/null >> \"$cache\"; then printf 'cache-seed:write-failed\\n' >&2; exit 93; fi; i=$((i + 1)); done".into(),
        ],
        cwd: String::new(),
    };
    let seed_requests = Arc::new(Mutex::new(Vec::new()));
    let mut seed_engine = Engine::open(
        StateRoot::admit(&state_success).unwrap(),
        RuntimeJourney {
            requests: seed_requests,
            calls: vec![cache_seed.clone()],
            resource_class: arany::ToolResourceClass::Default,
        },
    )
    .unwrap();
    let (seed_approvals, mut seed_inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    seed_engine
        .enable_trusted_workspace(
            permissions.clone(),
            &workspace,
            env!("CARGO_BIN_EXE_arany").into(),
            seed_approvals,
        )
        .unwrap();
    let seed_run = seed_engine.run(RunRequest {
        session_id: None,
        title: Some("Private cache seed".into()),
        objective: "Seed a bounded private cache archive".into(),
        images: Vec::new(),
        workspace: workspace.clone(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::default(),
    });
    let approve_seed = async {
        let approval = seed_inbox
            .next()
            .await
            .expect("private cache seed approval");
        assert!(
            approval.intent.call == cache_seed,
            "private cache seed approval intent differs"
        );
        approval.decide(true);
    };
    let (seed_outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(seed_run, approve_seed)
    })
    .await
    .expect("bounded private cache seed");
    let seed_outcome = seed_outcome.unwrap();
    seed_engine.close().await.unwrap();
    assert_eq!(
        seed_outcome.run.status,
        RunStatus::Finished,
        "cache seed Run failed: {}",
        crate::safe_run_diagnostics(&seed_outcome.run)
    );
    let seed_result: serde_json::Value = serde_json::from_str(
        &seed_outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .output,
    )
    .unwrap();
    let seed_observation = seed_outcome.run.tools[0].observation.as_ref().unwrap();
    let seed_stderr = seed_result["stderr"].as_str().unwrap_or_default();
    let seed_stage = match seed_stderr {
        "cache-seed:cargo-home-missing\n" => "cargo-home-missing",
        "cache-seed:mkdir-failed\n" => "mkdir",
        "cache-seed:create-failed\n" => "create",
        "cache-seed:write-failed\n" => "write",
        "" => "empty",
        _ => "other",
    };
    let seed_trace = trace_snapshot(root);
    assert_eq!(
        seed_result["exit_code"],
        0,
        "cache seed disposition {:?}; Guard receipt {}; cache update {:?}; stdout bytes {}; stderr bytes {}; stderr stage {}; no space {}; permission denied {}; operation denied {}; missing command {}; trace {:?}",
        seed_observation.disposition,
        seed_observation.guard.is_some(),
        seed_observation.cache_update,
        seed_result["stdout"].as_str().map_or(0, str::len),
        seed_stderr.len(),
        seed_stage,
        seed_stderr.contains("No space left on device"),
        seed_stderr.contains("Permission denied"),
        seed_stderr.contains("Operation not permitted"),
        seed_stderr.contains("not found"),
        seed_trace,
    );
    assert_eq!(
        seed_outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .cache_update,
        arany::CacheUpdate::Published
    );

    let mut permissions =
        arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    let cargo_cache = permissions
        .runtime_profile_choices()
        .into_iter()
        .find(|option| option.label.starts_with("Cargo archive cache"))
        .expect("metadata-only Cargo cache candidate");
    assert!(!cargo_cache.selected);
    let selected_tokens = vec![node_token.clone(), cargo_cache.token];
    permissions
        .remember_profiles(true, arany::ApprovalMode::AutoEdits, &selected_tokens)
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();

    let accepted_requests = Arc::new(Mutex::new(Vec::new()));
    let cache_write = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "mkdir -p \"$CARGO_TARGET_DIR\"; printf '%s' '{RUNTIME_CACHE_CANARY}' > \"$CARGO_TARGET_DIR/persistent-marker\""
            ),
        ],
        cwd: String::new(),
    };
    let cache_read = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "set -eu; test \"$(cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = '{RUNTIME_CACHE_CANARY}'; cat \"$CARGO_TARGET_DIR/persistent-marker\"; printf '\\n'; test \"$(cat \"$CARGO_HOME/registry/cache/canary.crate\")\" = MACOS_HOST_CACHE_CANARY; cat \"$CARGO_HOME/registry/cache/canary.crate\"; printf '\\n'; node"
            ),
        ],
        cwd: String::new(),
    };
    let cache_overflow = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; mkdir -p \"$CARGO_TARGET_DIR\"; cache=\"$CARGO_TARGET_DIR/overflow-marker\"; : > \"$cache\"; i=0; while test \"$i\" -lt 32; do printf '%1048576s' '' >> \"$cache\"; i=$((i + 1)); done".into(),
        ],
        cwd: String::new(),
    };
    let accepted_calls = vec![cache_write, cache_read, cache_overflow];
    let mut engine = Engine::open(
        StateRoot::open_existing(&state_success).unwrap(),
        RuntimeJourney {
            requests: accepted_requests.clone(),
            calls: accepted_calls.clone(),
            resource_class: arany::ToolResourceClass::Build,
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    engine
        .enable_trusted_workspace(
            permissions,
            &workspace,
            env!("CARGO_BIN_EXE_arany").into(),
            approvals,
        )
        .unwrap();
    for (project_bytes, cache_bytes) in [
        (128 * 1024 * 1024 + 1, 64 * 1024 * 1024),
        (128 * 1024 * 1024, 64 * 1024 * 1024 + 1),
    ] {
        let mut goal = build_goal_request();
        goal.resource_settings.build_project_bytes = project_bytes;
        goal.resource_settings.build_cache_bytes = cache_bytes;
        let invalid = engine
            .run(RunRequest {
                session_id: None,
                title: Some("Rejected native storage granularity".into()),
                objective: "The unsupported resource values must not be dispatched".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal,
            })
            .await;
        assert!(matches!(invalid, Err(arany::EngineError::InvalidRequest)));
        assert!(accepted_requests.lock().unwrap().is_empty());
    }
    let run = engine.run(RunRequest {
        session_id: None,
        title: Some("Selected runtime success".into()),
        objective: "Persist private build-cache data between commands using the selected runtime"
            .into(),
        images: Vec::new(),
        workspace: workspace.clone(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: build_goal_request(),
    });
    let approve = async {
        for call in &accepted_calls {
            let approval = inbox
                .next()
                .await
                .expect("approval for a selected-runtime command");
            assert!(
                &approval.intent.call == call,
                "selected-runtime approval intent differs"
            );
            approval.decide(true);
        }
    };
    let (accepted, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(run, approve)
    })
    .await
    .expect("bounded selected-runtime success journey");
    let accepted = accepted.unwrap();
    engine.close().await.unwrap();
    assert_eq!(
        accepted.run.status,
        RunStatus::Finished,
        "selected runtime Run failed: {}",
        crate::safe_run_diagnostics(&accepted.run)
    );
    assert_eq!(accepted.run.tools.len(), 3);
    for tool in &accepted.run.tools {
        let observation = tool.observation.as_ref().expect("Build Tool receipt");
        let limits = &observation.intent.limits;
        assert_eq!(limits.resource_class, arany::ToolResourceClass::Build);
        assert_eq!(
            limits.resource_profile,
            arany::ToolResourceProfile::MacosSupervised
        );
        assert_eq!(limits.runtime_ms, 120_000);
        assert_eq!(limits.cache_transfer_ms, 120_000);
        assert_eq!(limits.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.max_processes, 32);
        assert_eq!(limits.project_bytes, 128 * 1024 * 1024);
        assert_eq!(limits.cache_requested_bytes, 64 * 1024 * 1024);
        assert!(limits.cache_bytes >= 40 * 1024 * 1024);
        assert!(limits.cache_bytes <= limits.cache_requested_bytes);
        assert!(limits.cache_candidate_bytes <= limits.cache_bytes);
        let receipt = observation.guard.as_ref().expect("Build Guard receipt");
        assert_eq!(receipt.limits, *limits);
        assert_eq!(
            receipt.enforcement_digest,
            observation.intent.enforcement_digest
        );
    }
    let accepted_observations = accepted
        .run
        .tools
        .iter()
        .map(|tool| {
            let observation = tool.observation.as_ref().unwrap();
            assert_eq!(observation.disposition, ToolDisposition::Succeeded);
            assert_eq!(observation.intent, tool.intent);
            serde_json::from_str::<serde_json::Value>(&observation.output).unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        accepted_observations[0]["stdout"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    assert_eq!(accepted_observations[0]["exit_code"], 0);
    assert_eq!(accepted_observations[1]["exit_code"], 0);
    assert_eq!(accepted_observations[2]["exit_code"], 0);
    assert_eq!(
        accepted.run.tools[2]
            .observation
            .as_ref()
            .unwrap()
            .cache_update,
        arany::CacheUpdate::Rejected
    );
    assert!(accepted.run.tools[..2].iter().all(|tool| {
        tool.observation
            .as_ref()
            .is_some_and(|observation| observation.cache_update == arany::CacheUpdate::Published)
    }));
    let expected_stdout =
        format!("{RUNTIME_CACHE_CANARY}\nMACOS_HOST_CACHE_CANARY\nruntime-profile-selected\n");
    assert!(
        accepted_observations[1]["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout == expected_stdout),
        "selected runtime cache output differs"
    );
    {
        let accepted_requests = accepted_requests.lock().unwrap();
        assert_eq!(accepted_requests.len(), 4);
        assert!(
            accepted_requests[0]
                .tools
                .as_ref()
                .unwrap()
                .observations
                .is_empty()
        );
        let first_output = &accepted_requests[1].tools.as_ref().unwrap().observations[0].output;
        let first_command: serde_json::Value = serde_json::from_str(first_output).unwrap();
        assert!(first_command["stdout"].as_str().is_some_and(str::is_empty));
        let overflow_observation = &accepted_requests[3].tools.as_ref().unwrap().observations[2];
        let overflow_output = &overflow_observation.output;
        let overflow_result: serde_json::Value = serde_json::from_str(overflow_output).unwrap();
        assert_eq!(overflow_result["exit_code"], 0);
        assert_eq!(
            overflow_observation.cache_update,
            arany::CacheUpdate::Rejected
        );
        let provider_observation = accepted_requests[2]
            .tools
            .as_ref()
            .unwrap()
            .observations
            .get(1)
            .expect("Provider request after explicit selected-cache read");
        assert_eq!(provider_observation.intent.call, accepted_calls[1]);
        assert_eq!(provider_observation.disposition, ToolDisposition::Succeeded);
        let provider_output: serde_json::Value =
            serde_json::from_str(&provider_observation.output).unwrap();
        assert!(
            provider_output["stdout"]
                .as_str()
                .is_some_and(|stdout| stdout == expected_stdout),
            "explicit selected-cache output missing from later Provider request"
        );
    }
    let state = StateRoot::open_existing(&root.join("state-success")).unwrap();
    let store = Store::open_read_only(state).unwrap();
    let events = store.load_session(accepted.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(accepted.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.len(), 1);
    assert!(
        replay.runs[0] == accepted.run,
        "closed cache replay differs"
    );
    let goal = replay.goals.first().expect("replayed Goal");
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    let report = goal.report.as_ref().expect("durable Goal report");
    assert!(
        report
            .evidence
            .iter()
            .any(|item| item == "Command sh exited 0."),
        "successful Command evidence remains in the Goal report: {report:?}"
    );
    assert!(
        report.unresolved.iter().any(|item| {
            item.contains("build cache update was rejected")
                && item.contains("previous cache was kept")
        }),
        "cache rejection is recorded as unresolved: {report:?}"
    );
    let finished = events
        .iter()
        .filter_map(|event| match &event.event {
            arany::Event::ToolFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 3);
    assert!(finished[0] == accepted.run.tools[0].observation.as_ref().unwrap());
    assert!(finished[1] == accepted.run.tools[1].observation.as_ref().unwrap());
    assert!(finished[2] == accepted.run.tools[2].observation.as_ref().unwrap());

    let mut permissions =
        arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::AutoEdits,
            std::slice::from_ref(&node_token),
        )
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    let verify_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; test \"$(/usr/bin/wc -c < \"$CARGO_HOME/registry/cache/private-bulk.crate\")\" -eq 41943040; test \"$(cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = ARANY_RUNTIME_CACHE_PERSISTENCE_CANARY; test ! -e \"$CARGO_TARGET_DIR/overflow-marker\"; printf 'cache-prior-retained\\n'; node".into(),
        ],
        cwd: String::new(),
    };
    let verify_requests = Arc::new(Mutex::new(Vec::new()));
    let mut verify_engine = Engine::open(
        StateRoot::open_existing(&state_success).unwrap(),
        RuntimeJourney {
            requests: verify_requests,
            calls: vec![verify_cache.clone()],
            resource_class: arany::ToolResourceClass::Default,
        },
    )
    .unwrap();
    let (verify_approvals, mut verify_inbox) =
        arany::ToolApprovals::new(arany::ApprovalMode::Request);
    verify_engine
        .enable_trusted_workspace(
            permissions,
            &workspace,
            env!("CARGO_BIN_EXE_arany").into(),
            verify_approvals,
        )
        .unwrap();
    let verify_run = verify_engine.run(RunRequest {
        session_id: None,
        title: Some("Cache overflow recovery".into()),
        objective: "Verify the previous private cache survived a rejected update".into(),
        images: Vec::new(),
        workspace: workspace.clone(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::default(),
    });
    let approve_verify = async {
        let approval = verify_inbox.next().await.expect("cache recovery approval");
        assert!(
            approval.intent.call == verify_cache,
            "cache recovery approval intent differs"
        );
        approval.decide(true);
    };
    let (verified, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(verify_run, approve_verify)
    })
    .await
    .expect("bounded cache recovery journey");
    let verified = verified.unwrap();
    verify_engine.close().await.unwrap();
    assert_eq!(verified.run.status, RunStatus::Finished);
    let retained = verified.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(retained.cache_update, arany::CacheUpdate::Published);
    let retained_result: serde_json::Value = serde_json::from_str(&retained.output).unwrap();
    assert_eq!(
        retained_result["stdout"],
        "cache-prior-retained\nruntime-profile-selected\n"
    );
    let store = Store::open_read_only(StateRoot::open_existing(&state_success).unwrap()).unwrap();
    let events = store.load_session(verified.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(verified.session_id, &events)
        .unwrap()
        .unwrap();
    assert!(
        replay.runs.as_slice() == std::slice::from_ref(&verified.run),
        "closed cache recovery replay differs"
    );
    let finished = events
        .iter()
        .filter_map(|event| match &event.event {
            arany::Event::ToolFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(finished.len() == 1 && finished[0] == retained);
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
        trace_case(root, case);
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
        let mut progress = arany::RunProgress::new();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
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
                        goal: arany::GoalRequest::default(),
                    },
                    cancellation,
                    progress.clone()
                ),
                decision
            )
        })
        .await;
        let (outcome, ()) = match result {
            Ok(outcome) => outcome,
            Err(_) => {
                let update = tokio::time::timeout(Duration::from_millis(10), progress.changed())
                    .await
                    .ok();
                let waiting =
                    tokio::time::timeout(Duration::from_millis(10), progress.waiting_changed())
                        .await
                        .ok()
                        .flatten();
                panic!(
                    "bounded stopped native command {case}: elapsed_ms={}, progress={:?}, queued={}, trace={}",
                    started.elapsed().as_millis(),
                    update.map(|update| (
                        update.sequence,
                        update.run.status,
                        update.run.tools.len()
                    )),
                    waiting.is_some(),
                    trace_snapshot(root)
                );
            }
        };
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        if cfg!(debug_assertions) {
            let trace = trace_snapshot(root);
            for stage in ["tool_admission", "catalogue"] {
                assert_eq!(
                    trace["stages"][stage]["state"], "complete",
                    "{case}: {trace}"
                );
            }
            if matches!(case, "overflow" | "mcp-version" | "receipt-loss") {
                for stage in [
                    "admission",
                    "connect",
                    "supervisor_storage",
                    "worker_ready",
                    "worker_go",
                    "supervisor_coalition",
                    "supervisor_detach",
                    "controller_coalition",
                    "controller_bootout",
                    "controller_storage",
                ] {
                    assert_eq!(
                        trace["stages"][stage]["state"], "complete",
                        "{case}: {trace}"
                    );
                }
            }
        }
        assert_eq!(
            requests.lock().unwrap().len(),
            if attested { 2 } else { 1 },
            "no automatic retry: {case}"
        );
        let expected_run_status = match case {
            "cancelled" => RunStatus::Cancelled,
            "mcp-version" => RunStatus::Finished,
            _ => RunStatus::Failed,
        };
        assert_eq!(
            outcome.run.status, expected_run_status,
            "{case}: {:?}",
            outcome.run.tools
        );
        assert_eq!(
            outcome.run.assistant_message.is_some(),
            case == "mcp-version",
            "{case} assistant answer"
        );
        assert!(!workspace.join("src/approval-output.txt").exists());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs[0], outcome.run, "closed replay: {case}");
        assert_eq!(
            replay.goals[0].status,
            match case {
                "cancelled" => arany::GoalStatus::Cancelled,
                "mcp-version" => arany::GoalStatus::Completed,
                _ => arany::GoalStatus::Failed,
            },
            "closed Goal replay: {case}"
        );
        if case == "overflow" {
            let contract = replay.goals[0]
                .plan_contract
                .as_ref()
                .expect("overflow command has a plan");
            assert!(matches!(
                &contract.success_criterion,
                arany::GoalSuccessCriterion::ToolCallSucceeded { call: planned }
                    if **planned == call
            ));
            assert!(
                replay.goals[0]
                    .report
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .iter()
                    .any(|item| item.contains("Command shell failed"))
            );
        }
        if case == "mcp-version" {
            assert!(
                replay.goals[0].plan_contract.is_none(),
                "MCP catalog inspection stays in the planless prelude"
            );
            assert!(
                replay.goals[0]
                    .report
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .iter()
                    .any(|item| item.contains("MCP catalog synthetic failed"))
            );
        }
        if case == "receipt-loss" {
            assert_eq!(
                replay.runs[0].tools[0]
                    .observation
                    .as_ref()
                    .unwrap()
                    .cache_update,
                arany::CacheUpdate::Uncertain,
                "selected cache update remains uncertain"
            );
            let unresolved = &replay.goals[0].report.as_ref().unwrap().unresolved;
            assert!(
                unresolved
                    .iter()
                    .any(|item| item.contains("uncertain build cache publication")),
                "cache uncertainty is durable in the Goal report: {unresolved:?}"
            );
            assert!(
                unresolved
                    .iter()
                    .all(|item| !item.contains("inspect Workspace state")),
                "cache uncertainty does not misdirect recovery to Workspace state"
            );
        }
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
        ("invalid_go", "admission"),
        ("early_go", "admission"),
        ("stale_workspace", "workspace"),
        ("resource_mismatch", "admission"),
        ("catalogue_tamper", "admission"),
        ("truncated_task", "admission"),
        ("malformed_task", "admission"),
        ("oversized_task", "admission"),
        ("maximum_task", "handshake"),
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
        let mut task_value = json!({"config":serde_json::from_slice::<serde_json::Value>(config_bytes).unwrap(),
            "intent":intent,"workspace":workspace,"guard_executable":env!("CARGO_BIN_EXE_arany")});
        if case == "catalogue_tamper" {
            task_value["system_programs"] =
                json!({"programs":[["/bin/launchctl",[0,0,0,0,0,0,0]]]});
        }
        let mut task = serde_json::to_vec(&task_value).unwrap();
        const TASK_BYTES_CAP: usize = 96 * 1024;
        match case {
            "malformed_task" => task = b"{".to_vec(),
            "maximum_task" => task.resize(TASK_BYTES_CAP, b' '),
            "oversized_task" => task.resize(TASK_BYTES_CAP + 1, b' '),
            _ => {}
        }
        if !matches!(case, "truncated_task" | "oversized_task") {
            task.push(b'\n');
        }
        if case == "early_go" {
            task.extend_from_slice(b"GO\n");
        }
        if case == "invalid_go" {
            task.extend_from_slice(b"NO\n");
        }
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

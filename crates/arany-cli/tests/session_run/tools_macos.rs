use crate::engine_support::run_with;
use crate::process::BoundedOutput;
use crate::{hash, private_file};
use arany::{
    Answer, CollaborationPolicy, CompactionRequest, CompactionResponse, Provider, ProviderError,
    ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus, SessionView,
    StateRoot, Store, ToolCall, ToolDisposition, ToolResourceProfile,
};
use arany::{EngineConfig, RunControls, ToolGrant};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "tools_macos/repository.rs"]
mod repository;

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
                        | "git"
                        | "refusal"
                        | "denied"
                        | "headless"
                        | "cancelled"
                        | "overflow"
                        | "mcp-version"
                        | "receipt-loss"
                        | "runtime-mounts"
                        | "large-snapshot"
                        | "depth-32"
                        | "depth-33"
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
                        | "rust-build"
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
#[async_trait::async_trait]
impl Provider for Journey {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "file-tool-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let planned = crate::planned(&request);

        let mut requests = self.requests.lock().unwrap();
        let step = requests.len().saturating_sub(usize::from(planned));
        let context = request
            .input
            .tools
            .as_ref()
            .ok_or(ProviderError::InvalidOutcome)?;
        let catalog: serde_json::Value = serde_json::from_str(context.catalog()).unwrap();
        assert_eq!(catalog["commands"][0]["name"], "shell");
        assert_eq!(catalog["mcp_servers"][0]["name"], "synthetic");
        assert_eq!(
            context
                .observations()
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
                ProviderOutcome::Answer(Answer {
                    text: "Created and edited synthetic files.".into(),
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
                                        | ToolCall::MultiEdit { .. }
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
                if !planned && let Some(contract) = plan_contract {
                    arany::test_support::scenario::plan_outcome(*contract)
                } else {
                    crate::tool_outcome(call)
                }
            },
        );
        requests.push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage::default(),
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

#[test]
#[ignore = "requires physical Rust sysroot; set ARANY_TEST_CARGO_REGISTRY for repository acceptance"]
fn guarded_offline_rust_build_uses_selected_runtime_and_sdk() {
    let sysroot = std::env::var_os("ARANY_TEST_RUST_SYSROOT")
        .expect("set ARANY_TEST_RUST_SYSROOT to one physical versioned macOS Rust sysroot");
    let sysroot = validate_rust_sysroot(Path::new(&sysroot));
    let registry = std::env::var_os("ARANY_TEST_CARGO_REGISTRY")
        .map(|path| std::fs::canonicalize(path).unwrap());
    native_journey_with_rust_sysroot(
        "tools::native_offline_rust_build_helper",
        Some(&sysroot),
        registry.as_deref(),
    );
}

pub(super) fn native_journey(helper: &str) {
    native_journey_with_rust_sysroot(helper, None, None);
}

fn native_journey_with_rust_sysroot(
    helper: &str,
    rust_sysroot: Option<&Path>,
    registry: Option<&Path>,
) {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let fixture_root = if rust_sysroot.is_some() {
        let root = std::fs::canonicalize(temp.path())
            .expect("physical temporary root")
            .join("fixture root 'quoted'");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        root
    } else {
        temp.path().to_path_buf()
    };
    for directory in [
        "account",
        "account/guard-trace",
        "home",
        "environment-state",
        "environment-data",
    ] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(fixture_root.join(directory))
            .unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", helper, "--ignored", "--nocapture"])
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", fixture_root.join("account"))
        .env(
            "ARANY_TEST_GUARD_TRACE",
            fixture_root.join("account/guard-trace"),
        )
        .env("ARANY_TEST_FILE_JOURNEY_ROOT", &fixture_root)
        .env("HOME", fixture_root.join("home"))
        .env("XDG_STATE_HOME", fixture_root.join("environment-state"))
        .env("XDG_DATA_HOME", fixture_root.join("environment-data"))
        .current_dir(&fixture_root);
    crate::process::isolate_host_authentication(&mut command, &fixture_root.join("home"));
    if let Some(rust_sysroot) = rust_sysroot {
        command.env("ARANY_TEST_RUST_SYSROOT", rust_sysroot);
    }
    if let Some(registry) = registry {
        command.env("ARANY_TEST_CARGO_REGISTRY", registry);
    }
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        command.bounded_output_for(
            Duration::from_secs(if registry.is_some() {
                1500
            } else if rust_sysroot.is_some() {
                1200
            } else {
                360
            }),
            32 * 1024,
        )
    }))
    .unwrap_or_else(|_| {
        panic!(
            "native journey observer failed: {}",
            trace_snapshot(&fixture_root)
        )
    })
    .unwrap();
    assert!(
        output.status.success(),
        "native file journey: {:?}; stdout={:?}; stderr={:?}; trace={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        trace_snapshot(&fixture_root)
    );
    crate::process::assert_single_test_completed(&output.stdout, "native file journey");
    if rust_sysroot.is_some() {
        for line in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("offline Cargo ") || line.starts_with("repository "))
        {
            println!("{line}");
        }
    }
    assert!(
        output.stderr.is_empty(),
        "native journey stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn validate_rust_sysroot(path: &Path) -> std::path::PathBuf {
    assert!(path.is_absolute(), "Rust sysroot path must be absolute");
    let canonical = std::fs::canonicalize(path).expect("physical Rust sysroot");
    assert_eq!(
        canonical, path,
        "Rust sysroot path must already be physical"
    );
    for executable in ["bin/rustc", "bin/cargo"] {
        let metadata = std::fs::metadata(canonical.join(executable)).unwrap();
        assert!(metadata.is_file());
        assert_ne!(metadata.permissions().mode() & 0o111, 0);
    }
    let output = setup_context("Rust toolchain version probe", || {
        std::process::Command::new(canonical.join("bin/rustc"))
            .args(["--version", "--verbose"])
            .env_clear()
            .current_dir(&canonical)
            .bounded_output_for(Duration::from_secs(10), 4096)
            .expect("bounded Rust toolchain version probe")
    });
    assert!(
        output.status.success(),
        "Rust toolchain version probe failed: {output:?}"
    );
    let details = String::from_utf8(output.stdout).expect("Rust version output is UTF-8");
    assert!(output.stderr.is_empty());
    let field = |name: &str| {
        details
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("Rust version output lacks {name}"))
    };
    let release = field("release");
    let host = field("host");
    assert!(
        release.len() <= 64
            && release.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+')
            })
    );
    let expected_host = match std::env::consts::ARCH {
        "aarch64" => "aarch64-apple-darwin",
        "x86_64" => "x86_64-apple-darwin",
        architecture => panic!("unsupported native macOS architecture: {architecture}"),
    };
    assert_eq!(host, expected_host, "selected Rust sysroot host");
    assert_eq!(
        canonical.file_name().and_then(std::ffi::OsStr::to_str),
        Some(format!("{release}-{host}").as_str()),
        "Rust sysroot directory must be its exact versioned toolchain name"
    );
    canonical
}

fn assert_apple_runtime_baseline(root: &Path, sdk: &Path) {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let sdk_settings = sdk.join("SDKSettings.plist");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(sdk_settings)
        .expect("baseline physical SDK settings file is readable");
    let metadata = file.metadata().unwrap();
    assert!(metadata.is_file() && (1..=1024 * 1024).contains(&metadata.len()));
    let mut first_byte = [0];
    file.read_exact(&mut first_byte)
        .expect("bounded SDK settings read");

    let compiler = setup_context("baseline clang --version", || {
        std::process::Command::new("/Library/Developer/CommandLineTools/usr/bin/clang")
            .arg("--version")
            .env_clear()
            .env("HOME", root.join("home"))
            .env("TMPDIR", root)
            .current_dir(root)
            .bounded_output_for(Duration::from_secs(5), 4096)
            .expect("bounded baseline Command Line Tools invocation")
    });
    assert!(
        compiler.status.success(),
        "baseline Command Line Tools invocation failed: status {}",
        compiler.status
    );
    assert!(!compiler.stdout.is_empty() || !compiler.stderr.is_empty());

    let neighbor = setup_context("baseline llvm-nm --version", || {
        std::process::Command::new("/Library/Developer/CommandLineTools/usr/bin/llvm-nm")
            .arg("--version")
            .env_clear()
            .env("HOME", root.join("home"))
            .env("TMPDIR", root)
            .current_dir(root)
            .bounded_output_for(Duration::from_secs(5), 4096)
            .expect("bounded baseline Command Line Tools neighbor invocation")
    });
    assert!(
        neighbor.status.success(),
        "baseline Command Line Tools neighbor invocation failed: status {}",
        neighbor.status
    );
    assert!(!neighbor.stdout.is_empty() || !neighbor.stderr.is_empty());
}

fn setup_context<T>(stage: &str, operation: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        panic!("offline Rust build setup failed at {stage}: {message}");
    })
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
            offset: None,
            limit: None,
        },
        ToolCall::Edit {
            path: "src/main.txt".into(), expected_digest: hash(b"before\n"),
            old: "before".into(), new: "first".into(),
        },
        ToolCall::MultiEdit {
            file_path: "src/main.txt".into(),
            expected_digest: hash(b"first\n"),
            edits: vec![
                arany::LiteralEdit { old_string: "first".into(), new_string: "middle".into(), replace_all: false },
                arany::LiteralEdit { old_string: "middle".into(), new_string: "after".into(), replace_all: false },
            ],
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
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
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
            offset: None,
            limit: None,
        },
    ];
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Journey {
        requests: requests.clone(),
        calls: calls.clone(),
        linked_file: Some((root.join("outside.txt"), workspace.join("src/linked.txt"))),
    });
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let outcome = run_with(
        &engine,
        &provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some("Native files".into()),
            objective: "Complete the synthetic native file journey".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::default(),
        },
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
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
        assert_eq!(requests.len(), calls.len() + 2);
        for request in requests.iter() {
            assert_eq!(request.phase, arany::AgentPhase::RootPlan);
            assert_eq!(request.model, "file-tool-model");
            assert_eq!(
                request.input.objective,
                "Complete the synthetic native file journey"
            );
            assert!(
                request.input.workspace_guidance.is_empty()
                    && request.input.includes.is_empty()
                    && request.input.history.is_empty()
                    && request.input.context_summary.is_none()
                    && request.input.images.is_empty()
                    && request.input.child_results.is_empty()
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
        &outcome.run.tools[10..12]
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
        if let ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } = &observation.intent.call
        {
            assert_eq!(index, 11);
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
                14 | 15 => ToolDisposition::Conflict,
                16 => ToolDisposition::Denied,
                _ => ToolDisposition::Succeeded,
            }
        );
        assert_eq!(
            observation.intent.limits.resource_profile,
            ToolResourceProfile::MacosSupervised
        );
        match observation.attestation.as_ref().unwrap() {
            arany::Attestation::InProcess {
                receipt,
                policy_digest,
                workspace_device,
                workspace_inode,
            } => {
                assert!(matches!(
                    observation.intent.call,
                    ToolCall::Read { .. }
                        | ToolCall::Search { .. }
                        | ToolCall::Skill { .. }
                        | ToolCall::Write { .. }
                        | ToolCall::Edit { .. }
                        | ToolCall::MultiEdit { .. }
                        | ToolCall::Mkdir { .. }
                        | ToolCall::Delete { .. }
                        | ToolCall::Move { .. }
                        | ToolCall::ApplyChangeset { .. }
                ));
                if observation.disposition == ToolDisposition::Succeeded
                    && !matches!(
                        observation.intent.call,
                        ToolCall::Read { .. } | ToolCall::Search { .. } | ToolCall::Skill { .. }
                    )
                {
                    assert!(receipt.is_some());
                }
                assert_eq!(*policy_digest, observation.intent.policy_digest);
                assert_eq!(
                    (*workspace_device, *workspace_inode),
                    (
                        observation.intent.workspace_device,
                        observation.intent.workspace_inode
                    )
                );
            }
            arany::Attestation::Guard(receipt) => {
                assert_eq!(receipt.contract_version, 1);
                assert_eq!(
                    receipt.enforcement_digest,
                    observation.intent.enforcement_digest
                );
                assert_eq!(receipt.limits, observation.intent.limits);
            }
            arany::Attestation::Unguarded(_) => panic!("macOS journeys never run unguarded"),
            arany::Attestation::Host(_) => panic!("macOS journeys never run host Tools"),
        }
    }
    let command: serde_json::Value =
        serde_json::from_str(&run.tools[10].observation.as_ref().unwrap().output).unwrap();
    assert_eq!(command["exit_code"], 0);
    assert_eq!(command["stdout"], "native-command-success");
    assert_eq!(command["stderr"], "");
    assert_eq!(command["changeset"]["files"].as_array().unwrap().len(), 3);
    let mcp: serde_json::Value =
        serde_json::from_str(&run.tools[13].observation.as_ref().unwrap().output).unwrap();
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
    let fresh_provider = Arc::new(Journey {
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: vec![ToolCall::Command {
                program: "shell".into(),
                args: vec!["-c".into(), "set -eu; test \"$(/bin/cat src/command-output.txt)\" = 'command create'; test \"$(/bin/cat src/generated/note.txt)\" = 'command modify'; test ! -e src/deleted.txt; printf fresh-snapshot".into()],
                cwd: "".into(),
            }],
            linked_file: None,
        });
    let fresh = arany::test_support::engine::open(
        StateRoot::open_existing(&state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let fresh_outcome = run_with(
        &fresh,
        &fresh_provider,
        RunRequest {
            skill: None,
            session_id: Some(outcome.session_id),
            title: None,
            objective: "Verify a fresh native command snapshot".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::default(),
        },
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
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
    let discovery = outcome
        .run
        .tools
        .iter()
        .find(|tool| matches!(tool.intent.call, ToolCall::List { .. }))
        .expect("recorded Guard discovery intent");
    guard_refuses_before_go(root, &workspace, &discovery.intent);
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
    command_timeout_ms: Option<u32>,
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
#[async_trait::async_trait]
impl Provider for RuntimeJourney {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "runtime-profile-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let planned = crate::planned(&request);

        let step = {
            let mut requests = self.requests.lock().unwrap();
            let step = requests.len().saturating_sub(usize::from(planned));
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
                    command_timeout_ms: self.command_timeout_ms,
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
            if !planned && let Some(contract) = plan_contract {
                arany::test_support::scenario::plan_outcome(*contract)
            } else {
                crate::tool_outcome(call)
            }
        } else {
            ProviderOutcome::Answer(Answer { text: "The selected runtime command completed under Guard, and its private build cache persisted between commands.".into() })
        };
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage::default(),
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
#[ignore = "isolated helper owned by native offline Rust build journey"]
async fn native_offline_rust_build_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    trace_case(root, "rust-build");
    assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));

    let source_sysroot =
        std::env::var_os("ARANY_TEST_RUST_SYSROOT").expect("validated parent Rust sysroot");
    let source_sysroot = setup_context("source Rust sysroot validation", || {
        validate_rust_sysroot(Path::new(&source_sysroot))
    });
    let versioned_name = source_sysroot.file_name().unwrap();
    let rustup = root.join("home/.rustup");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&rustup)
        .unwrap();
    let toolchains = rustup.join("toolchains");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&toolchains)
        .unwrap();
    let copied = setup_context("private Rust sysroot clone", || {
        std::process::Command::new("/bin/cp")
            .args(["-cR"])
            .arg(&source_sysroot)
            .arg(&toolchains)
            .env_clear()
            .env("HOME", root.join("home"))
            .env("TMPDIR", root)
            .current_dir(root)
            .bounded_output_for(Duration::from_secs(45), 4096)
            .expect("bounded private Rust sysroot clone")
    });
    assert!(copied.status.success(), "Rust sysroot clone: {copied:?}");
    assert!(copied.stdout.is_empty() && copied.stderr.is_empty());
    let rust_sysroot = toolchains.join(versioned_name);
    assert_eq!(
        setup_context("cloned Rust sysroot validation", || {
            validate_rust_sysroot(&rust_sysroot)
        }),
        rust_sysroot
    );

    if std::env::var_os("ARANY_TEST_CARGO_REGISTRY").is_some() {
        repository::run(root).await;
        return;
    }
    let workspace = root.join("workspace");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .unwrap();
    arany::test_support::scenario::offline_cargo_fixture(&root.join("home"), &workspace);
    const APPLE_COMPILER: &str = "/Library/Developer/CommandLineTools/usr";
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let choices = permissions.runtime_profile_choices();
    let rust = choices
        .iter()
        .find(|option| option.group == "Rust" && option.path == rust_sysroot)
        .expect("copied exact Rust sysroot candidate");
    let compiler = choices
        .iter()
        .find(|option| {
            option.group == "Command Line Tools compiler"
                && option.path == Path::new(APPLE_COMPILER)
        })
        .expect("physical Apple Command Line Tools compiler candidate");
    let sdk = choices
        .iter()
        .find(|option| option.group == "macOS SDK" && option.detected && option.selected)
        .expect("physical versioned macOS SDK candidate");
    assert!(rust.detected && compiler.detected && sdk.detected);
    setup_context("Apple runtime baseline probes", || {
        assert_apple_runtime_baseline(root, &sdk.path)
    });
    assert_eq!(rust.version.as_deref(), versioned_name.to_str());
    assert_eq!(compiler.version, None);
    assert!(sdk.version.is_some());
    let apple_sdk = sdk.path.to_str().unwrap();
    let rust_token = rust.token.clone();
    let compiler_token = compiler.token.clone();
    let sdk_token = sdk.token.clone();

    let mut permissions = permissions;
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::Request,
            std::slice::from_ref(&rust_token),
        )
        .unwrap();
    let rust_only = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let denied_apple = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "set -eu; if {{ IFS= read -r _; }} < '{apple_sdk}/SDKSettings.plist' 2>/dev/null; then exit 41; fi; if '{APPLE_COMPILER}/bin/clang' --version >/dev/null 2>&1; then exit 42; fi; printf 'unselected-apple-runtimes-denied\\n'"
            ),
        ],
        cwd: String::new(),
    };
    let mut requested = arany::test_support::scenario::request(
        &workspace,
        "Verify Apple compiler and SDK access remain denied without those profiles",
    );
    requested.title = Some("Unselected Apple runtimes".into());
    requested.include_paths.clear();
    requested.goal = build_goal_request();
    let (denied, denied_requests) = run_offline_build_case(
        &root.join("state-denied-apple"),
        requested,
        rust_only,
        denied_apple.clone(),
        60_000,
    )
    .await;
    assert_eq!(denied.run.status, RunStatus::Finished);
    assert_eq!(denied.run.tools.len(), 1);
    let denied_observation = denied.run.tools[0]
        .observation
        .as_ref()
        .expect("denied Apple runtime receipt");
    let denied_output: serde_json::Value =
        serde_json::from_str(&denied_observation.output).unwrap();
    assert_eq!(denied_output["exit_code"], 0);
    assert_eq!(
        denied_output["stdout"],
        "unselected-apple-runtimes-denied\n"
    );
    assert_guarded_build_observation(denied_observation);
    assert_provider_journal(&denied_requests, &denied_apple);
    assert_closed_build_replay(root, "state-denied-apple", &denied).await;

    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let selected = [rust_token, compiler_token, sdk_token];
    permissions
        .remember_profiles(true, arany::ApprovalMode::Request, &selected)
        .unwrap();
    arany::test_support::scenario::guarded_offline_cargo_journey(
        root,
        &workspace,
        Path::new(env!("CARGO_BIN_EXE_arany")),
    )
    .await;
}

async fn run_offline_build_case(
    state: &Path,
    request: RunRequest,
    permissions: arany::WorkspacePermissions,
    call: ToolCall,
    timeout_ms: u32,
) -> (arany::RunOutcome, Arc<Mutex<Vec<ProviderRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RuntimeJourney {
        requests: requests.clone(),
        calls: vec![call.clone()],
        resource_class: arany::ToolResourceClass::Build,
        command_timeout_ms: Some(timeout_ms),
    });
    let engine = arany::test_support::engine::open(
        StateRoot::admit(state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let run = run_with(
        &engine,
        &provider,
        request,
        RunControls {
            tools: ToolGrant::Trusted(permissions),
            approvals: Some(approvals),
            ..Default::default()
        },
    );
    let approve = async {
        let mut calls = Vec::new();
        while let Some(approval) = inbox.next().await {
            assert!(
                !approval.allow_auto_edits,
                "commands cannot offer file-edit remembering"
            );
            let matches = calls.is_empty() && approval.intent.call == call;
            calls.push(approval.intent.call.clone());
            approval.decide(matches);
            if calls.len() == 2 {
                break;
            }
        }
        calls
    };
    let (outcome, approved_calls) = tokio::time::timeout(
        Duration::from_millis(u64::from(timeout_ms)) + Duration::from_secs(60),
        async { tokio::join!(run, approve) },
    )
    .await
    .expect("bounded native offline build run");
    engine.close().await.unwrap();
    let outcome = outcome.unwrap_or_else(|error| panic!("native build Engine failed: {error}"));
    assert_eq!(approved_calls, [call], "one exact native build approval");
    (outcome, requests)
}

fn assert_guarded_build_observation(observation: &arany::ToolObservation) {
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert_eq!(
        observation.intent.limits.resource_class,
        arany::ToolResourceClass::Build
    );
    assert_eq!(
        observation.intent.limits.resource_profile,
        ToolResourceProfile::MacosSupervised
    );
    assert_eq!(observation.intent.limits.runtime_ms, 60_000);
    let receipt = observation
        .attestation
        .as_ref()
        .and_then(arany::Attestation::guard)
        .expect("native Rust build Guard receipt");
    assert_eq!(receipt.limits, observation.intent.limits);
    assert_eq!(
        receipt.enforcement_digest,
        observation.intent.enforcement_digest
    );
}

fn assert_provider_journal(requests: &Arc<Mutex<Vec<ProviderRequest>>>, call: &ToolCall) {
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for before_dispatch in &requests[..2] {
        assert!(
            before_dispatch
                .input
                .tools
                .as_ref()
                .unwrap()
                .observations()
                .is_empty()
        );
    }
    let second = requests[2].input.tools.as_ref().unwrap().observations();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].intent.call, *call);
    assert_eq!(second[0].disposition, ToolDisposition::Succeeded);
}

async fn assert_closed_build_replay(root: &Path, state_name: &str, outcome: &arany::RunOutcome) {
    let store =
        Store::open_read_only(StateRoot::open_existing(&root.join(state_name)).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.as_slice(), std::slice::from_ref(&outcome.run));
    assert_eq!(replay.runs[0].status, RunStatus::Finished);
    let goal = replay.goals.first().expect("replayed build Goal");
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    assert!(goal.report.as_ref().is_some_and(|report| {
        report
            .evidence
            .iter()
            .any(|item| item == "Command sh exited 0.")
    }));
    let finished = events
        .iter()
        .filter_map(|event| match &event.event {
            arany::Event::ToolFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0],
        outcome.run.tools[0].observation.as_ref().unwrap()
    );
}

struct MountedRuntimeImage(Option<std::path::PathBuf>);

impl Drop for MountedRuntimeImage {
    fn drop(&mut self) {
        if let Some(image) = self.0.take() {
            let _ = crate::process::detach_disk_image(&image, Duration::from_secs(30));
        }
    }
}

fn runtime_mount_diagnostics(root: &Path, mountpoint: &Path, image: &Path) -> String {
    let captured = std::panic::catch_unwind(|| {
        std::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "+D"])
            .arg(mountpoint)
            .arg("--")
            .arg(image)
            .env_clear()
            .current_dir(root)
            .bounded_output_for(Duration::from_secs(5), 2048)
    });
    match captured {
        Ok(Ok(output)) => format!(
            "status={}; stdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Ok(Err(error)) => format!("lsof unavailable: {error}"),
        Err(_) => "lsof exceeded its 5-second deadline or 2048-byte per-stream bound".into(),
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
    let mut mounted = MountedRuntimeImage(Some(image.clone()));

    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
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
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();

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
    let provider = Arc::new(RuntimeJourney {
        requests: refused_requests.clone(),
        calls: vec![runtime_call.clone()],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
    });
    let engine = arany::test_support::engine::open(
        StateRoot::admit(&refused_state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let engine_controls = RunControls {
        tools: ToolGrant::Trusted(permissions.clone()),
        approvals: Some(approvals.clone()),
        ..Default::default()
    };
    let run = run_with(
        &engine,
        &provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some("Mounted runtime refusal".into()),
            objective: "Check runtime mount topology".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::new(
                arany::GoalBudget::default(),
                arany::GoalPhase::Implementation,
            ),
        },
        engine_controls,
    );
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
    assert_eq!(refused_requests.lock().unwrap().len(), 3);
    let refused_observation = refused.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(refused_observation.disposition, ToolDisposition::Failed);
    assert!(refused_observation.attestation.is_none());
    assert_eq!(
        refused_observation.enforcement_cause,
        Some(arany::EnforcementCause::GuardBootstrap)
    );
    assert!(
        refused_observation
            .output
            .contains("runtime mount topology")
    );
    for before_dispatch in 0..2 {
        assert_eq!(
            refused_requests.lock().unwrap()[before_dispatch]
                .input
                .tools
                .as_ref()
                .unwrap()
                .observations()
                .len(),
            0
        );
    }
    assert_eq!(
        refused_requests.lock().unwrap()[2]
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations(),
        vec![refused_observation]
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
    // Fixture cleanup after the product assertions; its bounded retry is not a retry of the test.
    let fixture_image = mounted.0.as_ref().expect("mounted runtime fixture");
    let detached = crate::process::detach_disk_image(fixture_image, Duration::from_secs(30));
    assert!(
        detached.is_ok(),
        "hdiutil detach: {detached:?}; mount and backing image references (current-user visibility only): {}",
        runtime_mount_diagnostics(root, &mountpoint, &image)
    );
    let _ = mounted.0.take();
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
    let seed_engine_provider = Arc::new(RuntimeJourney {
        requests: seed_requests,
        calls: vec![cache_seed.clone()],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
    });
    let seed_engine = arany::test_support::engine::open(
        StateRoot::admit(&state_success).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (seed_approvals, mut seed_inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let seed_engine_controls = RunControls {
        tools: ToolGrant::Trusted(permissions.clone()),
        approvals: Some(seed_approvals.clone()),
        ..Default::default()
    };
    let seed_run = run_with(
        &seed_engine,
        &seed_engine_provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some("Private cache seed".into()),
            objective: "Seed a bounded private cache archive".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::default(),
        },
        seed_engine_controls,
    );
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
        seed_observation.attestation.is_some(),
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

    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
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
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();

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
    let provider = Arc::new(RuntimeJourney {
        requests: accepted_requests.clone(),
        calls: accepted_calls.clone(),
        resource_class: arany::ToolResourceClass::Build,
        command_timeout_ms: None,
    });
    let engine = arany::test_support::engine::open(
        StateRoot::open_existing(&state_success).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let engine_controls = RunControls {
        tools: ToolGrant::Trusted(permissions),
        approvals: Some(approvals.clone()),
        ..Default::default()
    };
    for (project_bytes, cache_bytes) in [
        (128 * 1024 * 1024 + 1, 64 * 1024 * 1024),
        (128 * 1024 * 1024, 64 * 1024 * 1024 + 1),
    ] {
        let mut goal = build_goal_request();
        goal.resource_settings.build_project_bytes = project_bytes;
        goal.resource_settings.build_cache_bytes = cache_bytes;
        let invalid = run_with(
            &engine,
            &provider,
            RunRequest {
                skill: None,
                session_id: None,
                title: Some("Rejected native storage granularity".into()),
                objective: "The unsupported resource values must not be dispatched".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                launch_directory: None,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal,
            },
            engine_controls.clone(),
        )
        .await;
        assert!(matches!(invalid, Err(arany::EngineError::InvalidRequest)));
        assert!(accepted_requests.lock().unwrap().is_empty());
    }
    let run = run_with(
        &engine,
        &provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some("Selected runtime success".into()),
            objective:
                "Persist private build-cache data between commands using the selected runtime"
                    .into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: build_goal_request(),
        },
        engine_controls,
    );
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
        let receipt = observation
            .attestation
            .as_ref()
            .and_then(arany::Attestation::guard)
            .expect("Build Guard receipt");
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
        assert_eq!(accepted_requests.len(), 5);
        assert!(
            accepted_requests[0]
                .input
                .tools
                .as_ref()
                .unwrap()
                .observations()
                .is_empty()
        );
        let first_output = &accepted_requests[2]
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations()[0]
            .output;
        let first_command: serde_json::Value = serde_json::from_str(first_output).unwrap();
        assert!(first_command["stdout"].as_str().is_some_and(str::is_empty));
        let overflow_observation = &accepted_requests[4]
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations()[2];
        let overflow_output = &overflow_observation.output;
        let overflow_result: serde_json::Value = serde_json::from_str(overflow_output).unwrap();
        assert_eq!(overflow_result["exit_code"], 0);
        assert_eq!(
            overflow_observation.cache_update,
            arany::CacheUpdate::Rejected
        );
        let provider_observations = accepted_requests[3]
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations();
        let provider_observation = provider_observations
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

    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::AutoEdits,
            std::slice::from_ref(&node_token),
        )
        .unwrap();
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let verify_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; test \"$(/usr/bin/wc -c < \"$CARGO_HOME/registry/cache/private-bulk.crate\")\" -eq 41943040; test \"$(cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = ARANY_RUNTIME_CACHE_PERSISTENCE_CANARY; test ! -e \"$CARGO_TARGET_DIR/overflow-marker\"; printf 'cache-prior-retained\\n'; node".into(),
        ],
        cwd: String::new(),
    };
    let verify_requests = Arc::new(Mutex::new(Vec::new()));
    let verify_engine_provider = Arc::new(RuntimeJourney {
        requests: verify_requests,
        calls: vec![verify_cache.clone()],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
    });
    let verify_engine = arany::test_support::engine::open(
        StateRoot::open_existing(&state_success).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (verify_approvals, mut verify_inbox) =
        arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let verify_engine_controls = RunControls {
        tools: ToolGrant::Trusted(permissions),
        approvals: Some(verify_approvals.clone()),
        ..Default::default()
    };
    let verify_run = run_with(
        &verify_engine,
        &verify_engine_provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some("Cache overflow recovery".into()),
            objective: "Verify the previous private cache survived a rejected update".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            launch_directory: None,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: arany::GoalRequest::default(),
        },
        verify_engine_controls,
    );
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
        "headless",
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
        if case == "headless" {
            private_file(
                &state_path.join("permissions.json"),
                br#"{"permissions":{"ask":["Bash"]}}"#,
            );
        }
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
                "printf payload-started; printf '%1048576s' ''"
            } else {
                "printf must-not-dispatch > src/approval-output.txt"
            };
            ToolCall::Command {
                program: "shell".into(),
                args: vec!["-c".into(), script.into()],
                cwd: "".into(),
            }
        };
        let provider = Arc::new(Journey {
            requests: requests.clone(),
            calls: vec![call.clone()],
            linked_file: None,
        });
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let attested = matches!(case, "overflow" | "mcp-version");
        let granted = matches!(case, "denied" | "cancelled").then(|| approvals.clone());
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
        let mut events = engine.subscribe();
        let mut hints = arany::RunHints::new();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                run_with(
                    &engine,
                    &provider,
                    RunRequest {
                        skill: None,
                        session_id: None,
                        title: Some(format!("Native {case}")),
                        objective: "Stop this synthetic native command".into(),
                        images: Vec::new(),
                        workspace: workspace.into(),
                        launch_directory: None,
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                        goal: arany::GoalRequest::default(),
                    },
                    RunControls {
                        tools: ToolGrant::Configured,
                        approvals: granted,
                        cancellation,
                        hints: Some(hints.clone()),
                        ..Default::default()
                    }
                ),
                decision
            )
        })
        .await;
        let (outcome, ()) = match result {
            Ok(outcome) => outcome,
            Err(_) => {
                let event = tokio::time::timeout(Duration::from_millis(10), events.next())
                    .await
                    .ok();
                let waiting =
                    tokio::time::timeout(Duration::from_millis(10), hints.waiting_changed())
                        .await
                        .ok()
                        .flatten();
                panic!(
                    "bounded stopped native command {case}: elapsed_ms={}, progress={:?}, queued={}, trace={}",
                    started.elapsed().as_millis(),
                    event.flatten().map(|event| match event {
                        arany::Tapped::Committed(event) =>
                            format!("{}:{}", event.sequence, event.event.kind()),
                        arany::Tapped::Resync { missed } => format!("missed:{missed}"),
                    }),
                    waiting.is_some(),
                    trace_snapshot(root)
                );
            }
        };
        // A lost receipt leaves the command's effect unknown: the Run journals no terminal Event.
        let outcome = match outcome {
            Ok(outcome) => {
                assert_ne!(
                    case, "receipt-loss",
                    "an unknown outcome never settles its Run"
                );
                Some(outcome)
            }
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
                if case == "receipt-loss" =>
            {
                None
            }
            Err(error) => panic!("{case}: unexpected Run error {error}"),
        };
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
            match case {
                "denied" | "headless" | "overflow" => 3,
                "cancelled" | "mcp-version" | "receipt-loss" => 2,
                _ => unreachable!(),
            },
            "no automatic retry: {case}"
        );
        let session_id = match &outcome {
            Some(outcome) => outcome.session_id,
            None => {
                arany::list_sessions(
                    StateRoot::open_existing(&state_path).unwrap(),
                    workspace.to_path_buf(),
                )
                .await
                .unwrap()[0]
                    .id
            }
        };
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = SessionView::replay(session_id, &events).unwrap().unwrap();
        let run = outcome
            .as_ref()
            .map_or_else(|| replay.runs[0].clone(), |outcome| outcome.run.clone());
        assert_eq!(replay.runs[0], run, "closed replay: {case}");
        assert_native_refusal_continues(case, &run, &requests.lock().unwrap());
        assert_eq!(
            events
                .iter()
                .any(|envelope| matches!(envelope.event, arany::Event::RunFinished { .. })),
            case != "receipt-loss",
            "{case}: terminal Event"
        );
        let expected_run_status = match case {
            "cancelled" => RunStatus::Cancelled,
            "mcp-version" | "overflow" => RunStatus::Finished,
            "receipt-loss" => RunStatus::Interrupted,
            _ => RunStatus::Failed,
        };
        assert_eq!(
            run.status,
            expected_run_status,
            "{case}: {} Tools",
            run.tools.len()
        );
        assert_eq!(
            run.assistant_message.is_some(),
            matches!(case, "mcp-version" | "overflow"),
            "{case} assistant answer"
        );
        assert!(!workspace.join("src/approval-output.txt").exists());
        assert_eq!(
            replay.goals[0].status,
            match case {
                "cancelled" | "receipt-loss" => arany::GoalStatus::Interrupted,
                "mcp-version" | "overflow" => arany::GoalStatus::Completed,
                _ => arany::GoalStatus::Failed,
            },
            "closed Goal replay: {case}"
        );
        if case == "overflow" {
            const EXPECTED_OUTPUT_BYTES: u64 = 1_048_591;
            let contract = replay.goals[0]
                .plan_contract
                .as_ref()
                .expect("overflow command has a plan");
            assert!(matches!(
                &contract.success_criterion,
                arany::GoalSuccessCriterion::ToolCallSucceeded { call: planned }
                    if **planned == call
            ));
            let observation = replay.runs[0].tools[0].observation.as_ref().unwrap();
            let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(result["stdout_total"]["bytes"], EXPECTED_OUTPUT_BYTES);
            let spill = observation
                .spill
                .as_ref()
                .expect("macOS retains excerpted output");
            assert_eq!(spill.bytes, EXPECTED_OUTPUT_BYTES);
            assert!(!spill.truncated);
            let file = state_path
                .join("spill")
                .join(replay.runs[0].id.to_string())
                .join(format!("{}.txt", spill.r#ref));
            let raw = std::fs::read(file).unwrap();
            assert_eq!(raw.len() as u64, spill.bytes);
            let padding = raw
                .strip_prefix(b"payload-started")
                .expect("retained output starts with the command prefix");
            assert!(padding.iter().all(|byte| *byte == b' '));
            assert!(
                result["stdout"]
                    .as_str()
                    .unwrap()
                    .contains(" lines omitted; ")
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
            // A Guard error after dispatch never attempts cache publication; the command outcome stays uncertain.
            assert_eq!(
                replay.runs[0].tools[0]
                    .observation
                    .as_ref()
                    .unwrap()
                    .cache_update,
                arany::CacheUpdate::NotAttempted,
                "lost receipt leaves selected cache publication unattempted"
            );
            assert!(
                replay.goals[0].report.is_none(),
                "an interrupted Goal has no closing report"
            );
        }
        let observation = replay.runs[0].tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            match case {
                "denied" | "headless" | "cancelled" => ToolDisposition::Denied,
                "overflow" => ToolDisposition::Succeeded,
                "mcp-version" => ToolDisposition::Failed,
                "receipt-loss" => ToolDisposition::Uncertain,
                _ => unreachable!(),
            },
            "{case}"
        );
        assert_eq!(observation.attestation.is_some(), attested, "{case}");
    }
}

fn assert_native_refusal_continues(case: &str, run: &arany::RunView, requests: &[ProviderRequest]) {
    use arany::permissions::{Decision, PermissionSource};
    let (label, cause) = match case {
        "denied" => ("user_declined", arany::ApprovalCause::Declined),
        "headless" => (
            "channel_unavailable",
            arany::ApprovalCause::ChannelUnavailable,
        ),
        _ => return,
    };
    assert_eq!(
        run.failure,
        Some(arany::RunFailure::CriterionUnmet),
        "{case}"
    );
    assert_eq!(run.tools.len(), 1, "{case}: no automatic retry");
    let tool = &run.tools[0];
    if case == "headless" {
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, Decision::Ask, "approval is required");
        assert_eq!(verdict.source, PermissionSource::UnavailableApproval);
        assert_eq!(verdict.basis, PermissionSource::Rule);
        assert!(!verdict.approved);
    }
    assert_eq!(
        tool.observation.as_ref().unwrap().approval_cause,
        Some(cause)
    );
    assert_eq!(
        run.agents[0].provider_calls.last().unwrap().disposition,
        arany::ProviderCallDisposition::Finished,
        "{case}: explanatory Provider call completes"
    );
    let observations = requests
        .last()
        .unwrap()
        .input
        .tools
        .as_ref()
        .unwrap()
        .observations();
    assert_eq!(observations.len(), 1, "{case}: correlated refusal");
    assert_eq!(observations[0].intent, tool.intent);
    let expected = format!("{label}: no operation was dispatched");
    assert_eq!(observations[0].output, expected);
    assert_eq!(tool.model_denial_output(), Some(expected.as_str()));
    assert_eq!(observations[0].disposition, ToolDisposition::Denied);
    assert!(observations[0].attestation.is_none());
}

fn guard_refuses_before_go(root: &Path, workspace: &Path, admitted: &arany::EffectIntent) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let config_bytes = br#"{"version":1,"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}"#;
    for (case, stage) in [
        ("eof", "handshake"),
        ("workspace_write", "admission"),
        ("unknown_task_field", "admission"),
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
        intent.policy_digest = Sha256::digest(config_bytes).into();
        intent.call = if case == "workspace_write" {
            ToolCall::Write {
                path: "src/never-dispatched.txt".into(),
                expected_digest: None,
                content: "must not be created".into(),
            }
        } else {
            ToolCall::List { path: "src".into() }
        };
        if case == "stale_workspace" {
            intent.workspace_inode += 1;
        }
        if case == "resource_mismatch" {
            intent.limits.resource_profile = ToolResourceProfile::LinuxKernel;
        }
        let mut task_value = json!({"config":serde_json::from_slice::<serde_json::Value>(config_bytes).unwrap(),
            "intent":intent,"definition":null,"worker":null,"handover":null,"workspace":workspace,"guard_executable":env!("CARGO_BIN_EXE_arany")});
        if case == "unknown_task_field" {
            task_value["unexpected"] = json!(false);
        }
        if case == "catalogue_tamper" {
            task_value["system_programs"] =
                json!({"programs":[["/bin/launchctl",[0,0,0,0,0,0,0]]]});
        }
        let mut task = serde_json::to_vec(&task_value).unwrap();
        const TASK_BYTES_CAP: usize = 1024 * 1024;
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

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS one-use approval rows and replay"
)]
fn native_approval_rows_preserve_one_use_and_request_mode_eligibility() {
    native_journey("tools::native_approval_rows_helper");
}

struct ApprovalRowProvider(ToolCall);

#[async_trait::async_trait]
impl Provider for ApprovalRowProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "approval-rows", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let outcome = if !crate::planned(&request) {
            arany::test_support::scenario::plan_outcome(arany::GoalPlanContract {
                mission: "Review one native file edit".into(),
                paths: vec!["src".into()],
                commands: Vec::new(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                    call: Box::new(self.0.clone()),
                },
                budget: arany::GoalBudget::default(),
            })
        } else if request
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations()
            .is_empty()
        {
            crate::tool_outcome(self.0.clone())
        } else {
            ProviderOutcome::Answer(Answer {
                text: "Approved native edit".into(),
            })
        };
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage::default(),
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
#[ignore = "isolated helper owned by native approval rows"]
async fn native_approval_rows_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    for (name, settings, allow, eligible) in [
        ("yes", None, true, true),
        ("no", None, false, true),
        (
            "rule",
            Some(json!({"permissions":{"ask":["Edit"]}})),
            false,
            false,
        ),
        ("settings", Some(json!({"permissions":{}})), false, false),
    ] {
        let workspace = root.join(format!("approval-{name}"));
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let state_path = root.join(format!("approval-state-{name}"));
        let state = StateRoot::admit(&state_path).unwrap();
        private_file(&state_path.join("tools.json"), br#"{"version":1,"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}"#);
        if let Some(settings) = settings {
            private_file(
                &state_path.join("permissions.json"),
                &serde_json::to_vec(&settings).unwrap(),
            );
        }
        let call = ToolCall::Write {
            path: "src/approved.txt".into(),
            expected_digest: None,
            content: "native approval\n".into(),
        };
        let provider = Arc::new(ApprovalRowProvider(call.clone()));
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let answer = async {
            let approval = inbox.next().await.expect("one native approval");
            assert_eq!(approval.intent.call, call);
            assert_eq!(approval.allow_auto_edits, eligible, "{name}");
            assert!(approval.unguarded.is_none());
            approval.decide(if allow {
                arany::ApprovalReply::AllowOnce
            } else {
                arany::ApprovalReply::Deny
            });
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(
                run_with(
                    &engine,
                    &provider,
                    RunRequest {
                        skill: None,
                        session_id: None,
                        title: None,
                        objective: "Review the proposed native edit".into(),
                        images: Vec::new(),
                        workspace: workspace.clone(),
                        launch_directory: None,
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                        goal: arany::GoalRequest::default(),
                    },
                    RunControls {
                        tools: ToolGrant::Configured,
                        approvals: Some(approvals.clone()),
                        ..Default::default()
                    }
                ),
                answer
            )
        })
        .await
        .expect("bounded native approval");
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        assert_eq!(workspace.join("src/approved.txt").exists(), allow, "{name}");
        assert_eq!(
            outcome.run.status,
            if allow {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            }
        );
        assert_native_approval_completion(&outcome.run, allow);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = arany::SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs[0], outcome.run);
        if !allow {
            assert!(
                replay.runs[0]
                    .tools
                    .last()
                    .unwrap()
                    .observation
                    .as_ref()
                    .unwrap()
                    .attestation
                    .is_none()
            );
        }
    }
}

fn assert_native_approval_completion(run: &arany::RunView, allow: bool) {
    assert_eq!(run.agents[0].provider_calls.len(), 3);
    assert_eq!(
        run.agents[0].provider_calls.last().unwrap().disposition,
        arany::ProviderCallDisposition::Finished
    );
    assert_eq!(
        run.failure,
        (!allow).then_some(arany::RunFailure::CriterionUnmet)
    );
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert_eq!(
        observation.disposition,
        if allow {
            ToolDisposition::Succeeded
        } else {
            ToolDisposition::Denied
        }
    );
    if !allow {
        assert_eq!(
            observation.output,
            "user_declined: no operation was dispatched"
        );
        assert!(run.assistant_message.is_none());
    }
}

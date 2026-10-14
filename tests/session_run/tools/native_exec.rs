use super::*;
use crate::{loopback::ChildGuard, process::BoundedOutput};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

const CACHE_CANARY: &str = "EXPLICIT_CACHE_OUTPUT_CANARY";
const PRIVATE_CACHE_CANARY: &str = "ARANY_PRIVATE_CACHE_PERSISTENCE_CANARY";

struct CacheCanary {
    requests: std::sync::Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    calls: Vec<ToolCall>,
}

impl Provider for CacheCanary {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "cache-canary-model"
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
                    mission: "Use the selected cache through authorized commands".into(),
                    paths: Vec::new(),
                    commands: vec![program],
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
                summary: "Selected cache output was explicit".into(),
                result: "The private build cache persisted between commands, and selected host-cache output appeared only after an explicit read.".into(),
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

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux selected-cache disclosure through an explicit guarded command"
)]
fn selected_cache_is_disclosed_only_after_explicit_command_output() {
    use std::os::unix::fs::DirBuilderExt;

    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    for name in ["account-home", "home", "state"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(temp.path().join(name))
            .unwrap();
    }
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let cache = home.join(".cargo/registry/cache");
    std::fs::create_dir_all(&cache).unwrap();
    private_file(&cache.join("canary.crate"), CACHE_CANARY.as_bytes());
    let test_executable = std::env::current_exe().unwrap();
    let guard_executable = Path::new(env!("CARGO_BIN_EXE_arany"));
    let mut helper = crate::isolation::native_account_isolated_test_command(
        temp.path(),
        &test_executable,
        guard_executable,
    );
    helper
        .args([
            "--exact",
            "tools::native_exec::selected_cache_canary_helper",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .current_dir(temp.path());
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let output = helper
            .bounded_output_for(Duration::from_secs(90), 32 * 1024)
            .unwrap();
        assert!(
            output.status.success(),
            "selected-cache canary: stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::process::assert_single_test_completed(&output.stdout, "selected-cache canary");
        assert!(output.stderr.is_empty());
    }));
    if let Err(error) = checked {
        let _ = temp.keep();
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "isolated helper owned by selected-cache canary integration"]
async fn selected_cache_canary_helper() {
    let root = std::env::var_os("ARANY_TEST_CACHE_CANARY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    let workspace = root.join("workspace");
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    assert_eq!(user.dir, root.join("account-home"), "private passwd home");
    let account = user.dir.join(".local/state/arany");
    assert_eq!(StateRoot::account_path().unwrap(), account);
    let mut permissions =
        arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    let options = permissions.runtime_profile_choices();
    let cache = options
        .iter()
        .find(|option| option.label.starts_with("Cargo archive cache"))
        .expect("metadata-only Cargo cache candidate");
    assert!(!cache.selected);
    let cache_token = cache.token.clone();
    permissions
        .remember_profiles(true, arany::ApprovalMode::AutoEdits, &[])
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();

    let state_path = root.join("state");
    let seed_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; /usr/bin/mkdir -p \"$CARGO_HOME/registry/cache\"; /usr/bin/dd if=/dev/zero of=\"$CARGO_HOME/registry/cache/private-bulk.crate\" bs=1048576 count=50 2>/dev/null".into(),
        ],
        cwd: ".".into(),
    };
    let seed_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut seed_engine = Engine::open(
        StateRoot::admit(&state_path).unwrap(),
        CacheCanary {
            requests: seed_requests.clone(),
            calls: vec![seed_cache.clone()],
        },
    )
    .unwrap();
    let (seed_approvals, mut seed_inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    seed_engine
        .enable_trusted_workspace(
            permissions.clone(),
            &workspace,
            PathBuf::from(env!("CARGO_BIN_EXE_arany")),
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
        assert_eq!(approval.intent.call, seed_cache);
        approval.decide(true);
    };
    let (seed_outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(seed_run, approve_seed)
    })
    .await
    .expect("bounded private cache seed");
    let seed_outcome = seed_outcome.unwrap();
    seed_engine.close().await.unwrap();
    assert_eq!(seed_outcome.run.status, RunStatus::Finished);
    let seed_observation = seed_outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(seed_observation.disposition, ToolDisposition::Succeeded);
    assert!(seed_observation.guard.is_some());
    let seed_result: serde_json::Value = serde_json::from_str(&seed_observation.output).unwrap();
    assert_eq!(seed_result["exit_code"], 0);

    let mut permissions =
        arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::AutoEdits,
            std::slice::from_ref(&cache_token),
        )
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    assert!(
        permissions
            .runtime_profile_choices()
            .iter()
            .any(|option| option.selected && option.label.starts_with("Cargo archive cache"))
    );

    let state = StateRoot::open_existing(&state_path).unwrap();
    let write_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "/usr/bin/mkdir -p \"$CARGO_TARGET_DIR\"; printf '%s' '{PRIVATE_CACHE_CANARY}' > \"$CARGO_TARGET_DIR/persistent-marker\""
            ),
        ],
        cwd: ".".into(),
    };
    let read_caches = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "test \"$(/usr/bin/cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = '{PRIVATE_CACHE_CANARY}'; /usr/bin/cat \"$CARGO_TARGET_DIR/persistent-marker\"; printf '\\n'; /usr/bin/cat \"$CARGO_HOME/registry/cache/canary.crate\""
            ),
        ],
        cwd: ".".into(),
    };
    let overflow_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; /usr/bin/mkdir -p \"$CARGO_TARGET_DIR\"; /usr/bin/dd if=/dev/zero of=\"$CARGO_TARGET_DIR/overflow-marker\" bs=1048576 count=16 2>/dev/null".into(),
        ],
        cwd: ".".into(),
    };
    let calls = vec![write_cache, read_caches, overflow_cache];
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut engine = Engine::open(
        state,
        CacheCanary {
            requests: requests.clone(),
            calls: calls.clone(),
        },
    )
    .unwrap();
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    engine
        .enable_trusted_workspace(
            permissions,
            &workspace,
            PathBuf::from(env!("CARGO_BIN_EXE_arany")),
            approvals,
        )
        .unwrap();
    let run = engine.run(RunRequest {
        session_id: None,
        title: Some("Selected cache canary".into()),
        objective: "Persist private build-cache data, then explicitly read the selected host cache"
            .into(),
        images: Vec::new(),
        workspace: workspace.clone(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::default(),
    });
    let approve = async {
        for call in &calls {
            let approval = inbox
                .next()
                .await
                .expect("approval for a selected-cache command");
            assert!(
                &approval.intent.call == call,
                "selected-cache approval intent differs"
            );
            approval.decide(true);
        }
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(run, approve)
    })
    .await
    .expect("bounded selected-cache approval journey");
    let outcome = outcome.unwrap();
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "cache canary Run failed: {}",
        crate::safe_run_diagnostics(&outcome.run)
    );
    assert_eq!(outcome.run.tools.len(), 3);
    let mut observations = Vec::new();
    let mut command_results = Vec::new();
    for tool in &outcome.run.tools {
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(observation.intent, tool.intent);
        let receipt = observation
            .guard
            .as_ref()
            .expect("successful cache command has a Guard receipt");
        let expected_digest: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
        assert_eq!(receipt.contract_version, 1);
        assert_eq!(receipt.intent_digest, expected_digest);
        assert_eq!(
            receipt.enforcement_digest,
            observation.intent.enforcement_digest
        );
        assert_eq!(receipt.limits, observation.intent.limits);
        command_results
            .push(serde_json::from_str::<serde_json::Value>(&observation.output).unwrap());
        observations.push(observation);
    }
    assert_eq!(command_results[0]["exit_code"], 0);
    assert!(
        command_results[0]["stderr"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    assert!(
        command_results[0]["stdout"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    assert_eq!(command_results[1]["exit_code"], 0);
    assert!(
        command_results[1]["stderr"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    let expected_stdout = format!("{PRIVATE_CACHE_CANARY}\n{CACHE_CANARY}");
    assert!(
        command_results[1]["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout == expected_stdout),
        "explicit selected-cache output differs"
    );
    assert!(
        observations[..2]
            .iter()
            .all(|observation| observation.cache_update == arany::CacheUpdate::Published)
    );
    assert_eq!(command_results[2]["exit_code"], 0);
    assert!(
        command_results[2]["stderr"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    assert!(
        command_results[2]["stdout"]
            .as_str()
            .is_some_and(str::is_empty)
    );
    assert_eq!(observations[2].cache_update, arany::CacheUpdate::Rejected);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let _initial_tools = requests[0]
            .tools
            .as_ref()
            .expect("first cache canary request includes Tool context");
        let initial = format!("{:?}", requests[0]);
        assert!(
            !initial.contains(CACHE_CANARY),
            "cache canary was present in the initial Provider request"
        );
        let first_output = &requests[1].tools.as_ref().unwrap().observations[0].output;
        let first_command: serde_json::Value = serde_json::from_str(first_output).unwrap();
        assert!(first_command["stdout"].as_str().is_some_and(str::is_empty));
        assert!(!first_output.contains(PRIVATE_CACHE_CANARY));
        assert!(
            requests[2].tools.as_ref().unwrap().observations[1]
                .output
                .contains(PRIVATE_CACHE_CANARY)
                && requests[2].tools.as_ref().unwrap().observations[1]
                    .output
                    .contains(CACHE_CANARY),
            "explicit cache output missing from the later Provider request"
        );
        let overflow_observation = &requests[3].tools.as_ref().unwrap().observations[2];
        let overflow = overflow_observation.output.as_str();
        let overflow_result: serde_json::Value = serde_json::from_str(overflow).unwrap();
        assert_eq!(overflow_result["exit_code"], 0);
        assert_eq!(
            overflow_observation.cache_update,
            arany::CacheUpdate::Rejected
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.len(), 1);
    assert!(
        replay.runs[0] == outcome.run,
        "closed cache canary replay differs"
    );
    let finished = events
        .iter()
        .filter_map(|event| match &event.event {
            arany::Event::ToolFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 3);
    assert!(
        finished[0] == observations[0]
            && finished[1] == observations[1]
            && finished[2] == observations[2],
        "ToolFinished cache observations differ"
    );

    let mut permissions =
        arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    permissions
        .remember_profiles(true, arany::ApprovalMode::AutoEdits, &[])
        .unwrap();
    let permissions = arany::WorkspacePermissions::open_for_profile_selection(&workspace).unwrap();
    let verify_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "set -eu; test \"$(/usr/bin/wc -c < \"$CARGO_HOME/registry/cache/private-bulk.crate\")\" -eq 52428800; test \"$(/usr/bin/cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = '{PRIVATE_CACHE_CANARY}'; test ! -e \"$CARGO_TARGET_DIR/overflow-marker\"; printf cache-prior-retained"
            ),
        ],
        cwd: ".".into(),
    };
    let verify_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut verify_engine = Engine::open(
        StateRoot::open_existing(&state_path).unwrap(),
        CacheCanary {
            requests: verify_requests,
            calls: vec![verify_cache.clone()],
        },
    )
    .unwrap();
    let (verify_approvals, mut verify_inbox) =
        arany::ToolApprovals::new(arany::ApprovalMode::Request);
    verify_engine
        .enable_trusted_workspace(
            permissions,
            &workspace,
            PathBuf::from(env!("CARGO_BIN_EXE_arany")),
            verify_approvals,
        )
        .unwrap();
    let verify_run = verify_engine.run(RunRequest {
        session_id: None,
        title: Some("Cache overflow recovery".into()),
        objective: "Verify the previous private cache survived a rejected update".into(),
        images: Vec::new(),
        workspace,
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
    assert_eq!(retained.disposition, ToolDisposition::Succeeded);
    assert_eq!(retained.cache_update, arany::CacheUpdate::Published);
    let retained_result: serde_json::Value = serde_json::from_str(&retained.output).unwrap();
    assert_eq!(retained_result["stdout"], "cache-prior-retained");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
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

fn request(pipe: &mut impl Read) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "native Tool request deadline");
        let mut chunk = [0; 4096];
        match pipe.read(&mut chunk) {
            Ok(0) => panic!("native Tool peer ended before the expected request"),
            Ok(count) => {
                assert!(bytes.len() + count <= 64 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|size| size.parse::<usize>().ok())
                        })
                        .expect("native request Content-Length");
                    if bytes.len() == end + 4 + length {
                        return bytes;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::yield_now()
            }
            Err(error) => panic!("native Tool peer read: {error}"),
        }
    }
}

#[test]
#[ignore = "native Linux shipped exec --tools, both native adapters, local TLS and actual Guard"]
fn shipped_exec_continues_after_guarded_command_and_replays_closed_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let shim = root.join("loopback.so");
    assert!(
        Command::new("/usr/bin/gcc")
            .args(["-Wall", "-Wextra", "-Werror", "-shared", "-fPIC"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-tls-loopback.c"))
            .args(["-ldl", "-o"])
            .arg(&shim)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    let cert = root.join("server.pem");
    let key = root.join("server.key");
    let ca = root.join("ca.pem");
    let ca_key = root.join("ca.key");
    let csr = root.join("server.csr");
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=arany-native-tool-fixture",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-keyout"
            ])
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-subj",
                "/CN=api.openai.com",
                "-addext",
                "subjectAltName=DNS:api.openai.com,DNS:api.anthropic.com",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout"
            ])
            .arg(&key)
            .arg("-out")
            .arg(&csr)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args(["x509", "-req", "-in"])
            .arg(&csr)
            .arg("-CA")
            .arg(&ca)
            .arg("-CAkey")
            .arg(&ca_key)
            .args([
                "-CAcreateserial",
                "-days",
                "1",
                "-copy_extensions",
                "copy",
                "-out"
            ])
            .arg(&cert)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    for profile in ["openai", "anthropic"] {
        let workspace = root.join(format!("workspace-{profile}"));
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        private_file(&workspace.join("src/check.sh"), b"set -eu\ntest -z \"${LD_PRELOAD-}\"\ntest -z \"${OPENAI_API_KEY-}\"\ntest -z \"${ANTHROPIC_API_KEY-}\"\nprintf 'native command passed'\n");
        private_file(&workspace.join("src/note.txt"), b"native note\n");
        let state = root.join(format!("state-{profile}"));
        StateRoot::admit(&state).unwrap();
        basic_config(&state);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut server = ChildGuard::new(
            Command::new("/usr/bin/openssl")
                .args(["s_server", "-quiet", "-accept"])
                .arg(format!("127.0.0.1:{port}"))
                .arg("-cert")
                .arg(&cert)
                .arg("-key")
                .arg(&key)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "native TLS fixture start deadline"
            );
            std::thread::yield_now();
        }
        let mut output = server.child().stdout.take().unwrap();
        let flags = rustix::fs::fcntl_getfl(&output).unwrap();
        rustix::fs::fcntl_setfl(&output, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut input = server.child().stdin.take().unwrap();
        let responder = std::thread::spawn(move || {
            let model = if profile == "openai" {
                "gpt-5.4"
            } else {
                "claude-sonnet-5"
            };
            let mut contexts = Vec::new();
            for step in 0..4 {
                let bytes = request(&mut output);
                let end = bytes
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let header = std::str::from_utf8(&bytes[..end]).unwrap();
                assert!(header.starts_with(if profile == "openai" {
                    "POST /v1/responses HTTP/1.1\r\n"
                } else {
                    "POST /v1/messages HTTP/1.1\r\n"
                }));
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer synthetic-native-key\r\n")
                );
                let body: Value = serde_json::from_slice(&bytes[end..]).unwrap();
                assert_eq!(body["model"], model);
                let context: Value = serde_json::from_str(if profile == "openai" {
                    body["input"][0]["content"].as_str().unwrap()
                } else {
                    body["messages"][0]["content"].as_str().unwrap()
                })
                .unwrap();
                assert_eq!(context["phase"], "root_plan");
                assert_eq!(context["collaboration"], json!({"mode":"single"}));
                assert_eq!(context["objective"], "Native Tool CLI");
                assert_eq!(context["history"], json!([]));
                assert_eq!(context["includes"], json!([]));
                assert!(context["workspace_guidance"].is_null());
                let observations: Vec<Value> = if profile == "openai" {
                    assert!(context["tools"].get("observations").is_none());
                    let items = body["input"].as_array().unwrap();
                    assert_eq!(items.len(), 1 + step * 2);
                    items[1..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .enumerate()
                        .map(|(index, pair)| {
                            assert_eq!(pair[0]["type"], "function_call");
                            assert_eq!(pair[1]["type"], "function_call_output");
                            assert_eq!(pair[0]["call_id"], format!("arany_action_{index}"));
                            assert_eq!(pair[1]["call_id"], pair[0]["call_id"]);
                            serde_json::from_str(pair[1]["output"].as_str().unwrap()).unwrap()
                        })
                        .collect()
                } else {
                    context["tools"]["observations"].as_array().unwrap().clone()
                };
                assert_eq!(observations.len(), step);
                if step == 1 {
                    assert_eq!(observations[0]["disposition"], "succeeded");
                    assert_eq!(observations[0]["workspace_effect"], "none");
                    assert!(
                        observations[0].get("intent").is_none()
                            && observations[0].get("guard").is_none()
                    );
                    let command = observations[0]["output"].clone();
                    assert_eq!(
                        command,
                        json!({"exit_code":0,"stdout":"native command passed","stderr":"","changeset":{"files":[]}})
                    );
                }
                assert_eq!(context["tools"]["catalog"]["write"], true);
                let outcome = match step {
                    0 => {
                        json!({"outcome":{"type":"tool","call":{"operation":"command","program":"bash","args":["src/check.sh"],"cwd":""}}})
                    }
                    1 => {
                        json!({"outcome":{"type":"tool","call":{"operation":"read","path":"src/note.txt","offset":0,"limit":4096}}})
                    }
                    2 => {
                        assert_eq!(observations[1]["call"]["operation"], "read");
                        assert_eq!(observations[1]["workspace_effect"], "none");
                        assert_eq!(observations[1]["output"]["text"], "native note\n");
                        assert_eq!(observations[1]["output"]["sha256"], hash(b"native note\n"));
                        json!({"outcome":{"type":"tool","call":{"operation":"edit","path":"src/note.txt","expected_digest":observations[1]["output"]["sha256"],"old":"native note\n","new":"native note\n2026-10-06\n"}}})
                    }
                    3 => {
                        assert_eq!(observations[2]["call"]["operation"], "edit");
                        assert_eq!(observations[2]["disposition"], "succeeded");
                        assert_eq!(observations[2]["workspace_effect"], "applied");
                        assert_eq!(
                            observations[2]["output"]["sha256"],
                            hash(b"native note\n2026-10-06\n")
                        );
                        json!({"outcome":{"type":"finish","summary":"command and edit complete","result":"Native command and edit passed."}})
                    }
                    _ => unreachable!(),
                };
                contexts.push(context);
                let response = if profile == "openai" {
                    assert_eq!(body["tool_choice"], "required");
                    assert_eq!(body["parallel_tool_calls"], false);
                    assert_eq!(body["input"].as_array().unwrap().len(), 1 + step*2);
                    let mut arguments = if outcome["outcome"]["type"] == "tool" { outcome["outcome"]["call"].clone() } else { outcome["outcome"].clone() };
                    let key = if outcome["outcome"]["type"] == "tool" { "operation" } else { "type" };
                    let operation = arguments.as_object_mut().unwrap().remove(key).unwrap();
                    json!({"id":format!("resp_native_{step}"),"status":"completed","model":model,"output":[{"type":"function_call","id":format!("fc_native_{step}"),"call_id":format!("call_native_{step}"),"name":format!("arany_{}", operation.as_str().unwrap()),"arguments":arguments.to_string(),"status":"completed"}],"usage":{"input_tokens":12,"output_tokens":8}})
                } else {
                    json!({"id":format!("msg_native_{step}"),"type":"message","role":"assistant","model":model,"stop_reason":"end_turn","stop_sequence":null,"content":[{"type":"text","text":outcome.to_string()}],"usage":{"input_tokens":12,"output_tokens":8}})
                }.to_string();
                write!(input, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                input.flush().unwrap();
            }
            contexts
        });
        let output = Command::new(env!("CARGO_BIN_EXE_arany"))
            .args([
                "exec",
                "--tools",
                "--provider",
                profile,
                "--model",
                if profile == "openai" {
                    "gpt-5.4"
                } else {
                    "claude-sonnet-5"
                },
                "--effort",
                "low",
                "--collaboration",
                "single",
                "--output",
                "jsonl",
            ])
            .arg("--state-dir")
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .arg("Native Tool CLI")
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LD_PRELOAD", &shim)
            .env("ARANY_TEST_TLS_PORT", port.to_string())
            .env("SSL_CERT_FILE", &ca)
            .env(
                if profile == "openai" {
                    "OPENAI_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                },
                "synthetic-native-key",
            )
            .bounded_output()
            .unwrap();
        server.child().kill().unwrap();
        server.child().wait().unwrap();
        assert!(
            output.status.success(),
            "native exec failed: stderr={} events={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(4096)
                .flat_map(char::escape_default)
                .collect::<String>()
        );
        let contexts = responder.join().expect("strict native request semantics");
        assert!(output.stderr.is_empty());
        let lines: Vec<Value> = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let session_id = serde_json::from_value(lines[0]["session_id"].clone()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(session_id).await.unwrap();
            let run = store
                .load_view(session_id)
                .await
                .unwrap()
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(run.status, RunStatus::Finished);
            assert_eq!(
                run.assistant_message.as_deref(),
                Some("Native command and edit passed.")
            );
            assert_eq!(run.tools.len(), 3);
            assert!(
                run.tools
                    .iter()
                    .all(|tool| tool.observation.as_ref().unwrap().disposition
                        == ToolDisposition::Succeeded)
            );
            assert_eq!(
                std::fs::read(workspace.join("src/note.txt")).unwrap(),
                b"native note\n2026-10-06\n"
            );
            assert_eq!(contexts.len(), 4);
            assert_eq!(run.agents[0].provider_calls.len(), 4);
            assert_eq!(
                output.stdout,
                arany::render_exec(&run, &events, arany::Output::Jsonl, true).into_bytes()
            );
            store.close().await.unwrap();
        });
    }
}

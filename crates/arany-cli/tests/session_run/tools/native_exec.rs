use super::*;
#[cfg(target_os = "linux")]
#[path = "native_exec/cache_overlay.rs"]
mod cache_overlay;
#[cfg(target_os = "linux")]
#[path = "native_exec/overlay.rs"]
mod overlay;
#[cfg(target_os = "linux")]
#[path = "native_exec/supervisor.rs"]
mod supervisor;
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

fn cache_progress(stage: &str) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "cache-canary:{stage}").expect("cache fixture progress write");
    stdout.flush().expect("cache fixture progress flush");
}

fn exec_failure_summary(output: &std::process::Output, provider_finished: bool) -> String {
    const PHASES: &[&str] = &["root_plan", "tool_review", "child_work", "root_synthesis"];
    const DISPOSITIONS: &[&str] = &[
        "finished",
        "delegated",
        "tool_requested",
        "unavailable",
        "rejected",
        "invalid_response",
        "output_limit",
        "timed_out",
        "cancelled",
        "task_panic",
    ];
    const FAILURE_REASONS: &[&str] = &[
        "account_access",
        "usage_limit",
        "usage_temporarily_unavailable",
        "service_unavailable",
        "stream_protocol",
        "response_contract",
        "outcome_contract",
        "local_output_limit",
    ];
    const RUN_STATUSES: &[&str] = &["finished", "failed", "cancelled"];
    let mut provider_calls = Vec::new();
    let mut run_statuses = Vec::new();
    for line in output.stdout.split(|byte| *byte == b'\n').take(128) {
        if line.is_empty() || line.len() > 64 * 1024 {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(kind) = event["kind"].as_str() else {
            continue;
        };
        if kind == "RunFinished" && run_statuses.len() < 8 {
            run_statuses.push(enum_label(
                event["payload"]["disposition"].as_str(),
                RUN_STATUSES,
            ));
        }
        if kind == "ProviderCallRecorded" && provider_calls.len() < 32 {
            let payload = &event["payload"];
            let phase = enum_label(payload["phase"].as_str(), PHASES);
            let disposition = enum_label(payload["disposition"].as_str(), DISPOSITIONS);
            let failure = if payload["failure_reason"].is_null() {
                "none"
            } else {
                enum_label(payload["failure_reason"].as_str(), FAILURE_REASONS)
            };
            provider_calls.push(format!("{phase}:{disposition}:{failure}"));
        }
    }
    format!(
        "exit={:?}, stderr_present={}, provider_peer_completed={}, run_statuses={run_statuses:?}, provider_calls={provider_calls:?}",
        output.status.code(),
        !output.stderr.is_empty(),
        provider_finished,
    )
}

fn enum_label(value: Option<&str>, allowed: &[&'static str]) -> &'static str {
    allowed
        .iter()
        .copied()
        .find(|candidate| Some(*candidate) == value)
        .unwrap_or("unknown")
}

fn cache_public_failure_stage(output: &str) -> &'static str {
    match output.split(" Cause:").next().unwrap_or(output) {
        "Guard admission unavailable before dispatch; no operation was dispatched." => "admission",
        "Guard pinned input unavailable before dispatch; no operation was dispatched." => {
            "pinned-input"
        }
        "Guard native protection unavailable before dispatch; no operation was dispatched." => {
            "native-protection"
        }
        "Guard resource limit unavailable before dispatch; no operation was dispatched." => {
            "resource-limit"
        }
        "Guard execution unavailable before dispatch; no operation was dispatched." => "execution",
        "Guard runtime mount topology unavailable before dispatch; no operation was dispatched." => {
            "runtime-mounts"
        }
        "Guard handshake unavailable before dispatch; no operation was dispatched." => "handshake",
        "Guard receipt unavailable before dispatch; no operation was dispatched." => "receipt",
        "Guard resource attestation unavailable before dispatch; no operation was dispatched." => {
            "resource-attestation"
        }
        "Guard process cleanup unavailable before dispatch; no operation was dispatched." => {
            "process-cleanup"
        }
        "Guard storage cleanup unavailable before dispatch; no operation was dispatched." => {
            "storage-cleanup"
        }
        "Guard launcher reap unavailable before dispatch; no operation was dispatched." => {
            "launcher-reap"
        }
        "Guard output EOF unavailable before dispatch; no operation was dispatched." => {
            "output-eof"
        }
        "Guard namespace bootstrap unavailable before dispatch; no operation was dispatched." => {
            "namespace-bootstrap"
        }
        "Guard native manager unavailable before dispatch; no operation was dispatched." => {
            "native-manager"
        }
        "Guard control protocol unavailable before dispatch; no operation was dispatched." => {
            "control-protocol"
        }
        "Guard project selection timed out before dispatch; no operation was dispatched." => {
            "selection-timed-out"
        }
        "Guard project input vanished before dispatch; no operation was dispatched." => {
            "selection-input-vanished"
        }
        "Guard project input conflict before dispatch; no operation was dispatched." => {
            "selection-input-conflict"
        }
        "Guard waiting queue is full; retry when capacity is available. No operation was dispatched." => {
            "queue-full"
        }
        _ => "other",
    }
}

fn cache_run_return(result: &Result<arany::RunOutcome, arany::EngineError>) -> String {
    match result {
        Ok(outcome) => {
            let receipts = outcome
                .run
                .tools
                .iter()
                .map(|tool| {
                    tool.observation.as_ref().map(|observation| {
                        (
                            observation.disposition,
                            observation.attestation.is_some(),
                            observation.cache_update,
                            cache_public_failure_stage(&observation.output),
                            observation.output.len(),
                        )
                    })
                })
                .collect::<Vec<_>>();
            format!(
                "Run status {:?}; tool receipts {receipts:?}",
                outcome.run.status
            )
        }
        Err(error) => {
            let category = match error {
                arany::EngineError::Store(_) => "store",
                arany::EngineError::Tool(_) => "tool",
                arany::EngineError::CoordinatorFailed => "coordinator",
                arany::EngineError::CancellationDrainTimeout => "cancellation-drain",
                arany::EngineError::CancelledBeforeStart => "cancelled",
                arany::EngineError::InvalidHistory => "history",
                _ => "request-or-state",
            };
            format!("EngineError category {category}")
        }
    }
}

struct CacheCanary {
    requests: std::sync::Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    calls: Vec<ToolCall>,
    stage: &'static str,
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

#[async_trait::async_trait]
impl Provider for CacheCanary {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "cache-canary-model",
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
        cache_progress(&format!("{}:provider-step-{step}", self.stage));
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
            if !planned && let Some(contract) = plan_contract {
                arany::test_support::scenario::plan_outcome(*contract)
            } else {
                crate::tool_outcome(call)
            }
        } else {
            ProviderOutcome::Answer(Answer { text: "The private build cache persisted between commands, and selected host-cache output appeared only after an explicit read.".into() })
        };
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            },
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
    Box::pin(selected_cache_canary_helper_inner()).await;
}

async fn selected_cache_canary_helper_inner() {
    cache_progress("helper-start");
    let root = std::env::var_os("ARANY_TEST_CACHE_CANARY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    let workspace = root.join("workspace");
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    assert_eq!(user.dir, root.join("account-home"), "private passwd home");
    let account = user.dir.join(".local/state/arany");
    assert_eq!(StateRoot::account_path().unwrap(), account);
    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
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
    cache_progress("seed-admission-complete");
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();

    let state_path = root.join("state");
    let seed_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "set -eu; /usr/bin/mkdir -p \"$CARGO_HOME/registry/cache\"; cache=\"$CARGO_HOME/registry/cache/private-bulk.crate\"; : > \"$cache\"; i=0; while test \"$i\" -lt 40; do printf '%1048576s' '' >> \"$cache\"; i=$((i + 1)); done".into(),
        ],
        cwd: ".".into(),
    };
    let seed_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seed_engine_provider = Arc::new(CacheCanary {
        requests: seed_requests.clone(),
        calls: vec![seed_cache.clone()],
        stage: "seed",
        resource_class: arany::ToolResourceClass::Default,
    });
    // This helper runs under a private passwd home and must use its real account root.
    let seed_engine = arany::Engine::open(
        StateRoot::admit(&state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_arany"))),
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
    let mut seed_run = Box::pin(async {
        cache_progress("seed-run-start");
        let outcome = run_with(
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
        )
        .await;
        cache_progress("seed-run-returned");
        outcome
    });
    let seed_outcome = tokio::time::timeout(Duration::from_secs(90), async {
        cache_progress("seed-approval-waiting");
        let approval = tokio::select! {
            result = &mut seed_run => panic!(
                "cache seed Run returned before approval: {}",
                cache_run_return(&result),
            ),
            approval = seed_inbox.next() => approval.expect("private cache seed approval"),
        };
        cache_progress("seed-approval-received");
        assert_eq!(approval.intent.call, seed_cache);
        approval.decide(true);
        seed_run.await
    })
    .await
    .expect("bounded private cache seed");
    let seed_outcome = seed_outcome.unwrap();
    seed_engine.close().await.unwrap();
    let seed_observation = seed_outcome
        .run
        .tools
        .first()
        .and_then(|tool| tool.observation.as_ref());
    let seed_result = seed_observation.and_then(|observation| {
        serde_json::from_str::<serde_json::Value>(&observation.output).ok()
    });
    assert_eq!(
        seed_outcome.run.status,
        RunStatus::Finished,
        "cache seed status {:?}; tool disposition {:?}; guard receipt {}; cache update {:?}; exit {:?}; stdout bytes {}; stderr bytes {}",
        seed_outcome.run.status,
        seed_observation.map(|observation| observation.disposition),
        seed_observation.is_some_and(|observation| observation.attestation.is_some()),
        seed_observation.map(|observation| observation.cache_update),
        seed_result
            .as_ref()
            .and_then(|result| result["exit_code"].as_i64()),
        seed_result
            .as_ref()
            .and_then(|result| result["stdout"].as_str())
            .map_or(0, str::len),
        seed_result
            .as_ref()
            .and_then(|result| result["stderr"].as_str())
            .map_or(0, str::len),
    );
    let seed_observation = seed_observation.unwrap();
    assert_eq!(seed_observation.disposition, ToolDisposition::Succeeded);
    assert!(seed_observation.attestation.is_some());
    let seed_result = seed_result.unwrap();
    assert_eq!(seed_result["exit_code"], 0);
    cache_progress("seed-complete");

    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::AutoEdits,
            std::slice::from_ref(&cache_token),
        )
        .unwrap();
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
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
            "set -eu; /usr/bin/mkdir -p \"$CARGO_TARGET_DIR\"; cache=\"$CARGO_TARGET_DIR/overflow-marker\"; : > \"$cache\"; i=0; while test \"$i\" -lt 32; do printf '%1048576s' '' >> \"$cache\"; i=$((i + 1)); done".into(),
        ],
        cwd: ".".into(),
    };
    let calls = vec![write_cache, read_caches, overflow_cache];
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = Arc::new(CacheCanary {
        requests: requests.clone(),
        calls: calls.clone(),
        stage: "selected",
        resource_class: arany::ToolResourceClass::Build,
    });
    let engine = arany::Engine::open(
        state,
        EngineConfig {
            guard_executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_arany"))),
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
    let mut run =
        Box::pin(async {
            cache_progress("selected-run-start");
            let outcome = run_with(&engine, &provider, RunRequest {
                skill: None,
                session_id: None,
                title: Some("Selected cache canary".into()),
                objective:
                    "Persist private build-cache data, then explicitly read the selected host cache"
                        .into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                launch_directory: None,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal: build_goal_request(),
            }, engine_controls)
            .await;
            cache_progress("selected-run-returned");
            outcome
        });
    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        for (index, call) in calls.iter().enumerate() {
            cache_progress(&format!("selected-approval-waiting-{index}"));
            let approval = tokio::select! {
                result = &mut run => panic!(
                    "selected-cache Run returned before approval {index}: {}",
                    cache_run_return(&result),
                ),
                approval = inbox.next() => approval.expect("approval for a selected-cache command"),
            };
            cache_progress(&format!("selected-approval-received-{index}"));
            assert!(
                &approval.intent.call == call,
                "selected-cache approval intent differs"
            );
            approval.decide(true);
        }
        run.await
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
    cache_progress("cache-commands-complete");
    let mut observations = Vec::new();
    let mut command_results = Vec::new();
    for tool in &outcome.run.tools {
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(observation.intent, tool.intent);
        let limits = &observation.intent.limits;
        assert_eq!(limits.resource_class, arany::ToolResourceClass::Build);
        assert_eq!(
            limits.resource_profile,
            arany::ToolResourceProfile::LinuxKernel
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
        assert_eq!(requests.len(), 5);
        let _initial_tools = requests[0]
            .input
            .tools
            .as_ref()
            .expect("first cache canary request includes Tool context");
        let initial = format!("{:?}", requests[0]);
        assert!(
            !initial.contains(CACHE_CANARY),
            "cache canary was present in the initial Provider request"
        );
        let first_output = &requests[2].input.tools.as_ref().unwrap().observations()[0].output;
        let first_command: serde_json::Value = serde_json::from_str(first_output).unwrap();
        assert!(first_command["stdout"].as_str().is_some_and(str::is_empty));
        assert!(!first_output.contains(PRIVATE_CACHE_CANARY));
        assert!(
            requests[3].input.tools.as_ref().unwrap().observations()[1]
                .output
                .contains(PRIVATE_CACHE_CANARY)
                && requests[3].input.tools.as_ref().unwrap().observations()[1]
                    .output
                    .contains(CACHE_CANARY),
            "explicit cache output missing from the later Provider request"
        );
        let overflow_observation = &requests[4].input.tools.as_ref().unwrap().observations()[2];
        let overflow = overflow_observation.output.as_str();
        let overflow_result: serde_json::Value = serde_json::from_str(overflow).unwrap();
        assert_eq!(overflow_result["exit_code"], 0);
        assert_eq!(
            overflow_observation.cache_update,
            arany::CacheUpdate::Rejected
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    cache_progress("closed-replay-start");
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
    cache_progress("closed-replay-complete");

    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    permissions
        .remember_profiles(true, arany::ApprovalMode::AutoEdits, &[])
        .unwrap();
    let permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let verify_cache = ToolCall::Command {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "set -eu; test \"$(/usr/bin/wc -c < \"$CARGO_HOME/registry/cache/private-bulk.crate\")\" -eq 41943040; test \"$(/usr/bin/cat \"$CARGO_TARGET_DIR/persistent-marker\")\" = '{PRIVATE_CACHE_CANARY}'; test ! -e \"$CARGO_TARGET_DIR/overflow-marker\"; printf cache-prior-retained"
            ),
        ],
        cwd: ".".into(),
    };
    let verify_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let verify_engine_provider = Arc::new(CacheCanary {
        requests: verify_requests,
        calls: vec![verify_cache.clone()],
        stage: "verify",
        resource_class: arany::ToolResourceClass::Default,
    });
    let verify_engine = arany::Engine::open(
        StateRoot::open_existing(&state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(PathBuf::from(env!("CARGO_BIN_EXE_arany"))),
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
    let mut verify_run = Box::pin(async {
        cache_progress("verify-run-start");
        let outcome = run_with(
            &verify_engine,
            &verify_engine_provider,
            RunRequest {
                skill: None,
                session_id: None,
                title: Some("Cache overflow recovery".into()),
                objective: "Verify the previous private cache survived a rejected update".into(),
                images: Vec::new(),
                workspace,
                launch_directory: None,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal: arany::GoalRequest::default(),
            },
            verify_engine_controls,
        )
        .await;
        cache_progress("verify-run-returned");
        outcome
    });
    let verified = tokio::time::timeout(Duration::from_secs(90), async {
        cache_progress("verify-approval-waiting");
        let approval = tokio::select! {
            result = &mut verify_run => panic!(
                "cache recovery Run returned before approval: {}",
                cache_run_return(&result),
            ),
            approval = verify_inbox.next() => approval.expect("cache recovery approval"),
        };
        cache_progress("verify-approval-received");
        assert!(
            approval.intent.call == verify_cache,
            "cache recovery approval intent differs"
        );
        approval.decide(true);
        verify_run.await
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
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let crate::tls_fixture::TlsFixture {
        shim,
        cert,
        key,
        ca,
    } = crate::tls_fixture::build(root);
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
                let lower_header = header.to_ascii_lowercase();
                assert!(lower_header.contains("authorization: bearer synthetic-native-key\r\n"));
                if profile != "openai" {
                    assert!(lower_header.contains("anthropic-version: 2023-06-01\r\n"));
                    assert!(!lower_header.contains("x-api-key:"));
                }
                let body: Value = serde_json::from_slice(&bytes[end..]).unwrap();
                assert_eq!(body["model"], model);
                let context = crate::semantic::input(&body);
                assert_eq!(context["phase"], "root_plan");
                assert_eq!(context["collaboration"], json!({"mode":"single"}));
                assert_eq!(context["objective"], "Native Tool CLI");
                assert_eq!(
                    context["goal_budget"],
                    Value::Null,
                    "a plain turn sends no budget block"
                );
                assert_eq!(
                    context["goal_plan_contract"],
                    Value::Null,
                    "the goal block stays as the Run began"
                );
                assert_eq!(context["history"], json!([]));
                assert_eq!(context["includes"], json!([]));
                assert!(context["workspace_guidance"].is_null());
                let observations = crate::semantic::tool_results(&body);
                if profile == "openai" {
                    assert!(
                        body["include"]
                            .as_array()
                            .unwrap()
                            .contains(&json!("reasoning.encrypted_content"))
                    );
                    let calls: Vec<_> = body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| item["type"] == "function_call")
                        .collect();
                    assert_eq!(calls.len(), step);
                    for (index, call) in calls.iter().enumerate() {
                        assert_eq!(call["call_id"], format!("call_native_{index}"));
                    }
                } else {
                    assert_eq!(body["tool_choice"], json!({"type": "auto"}));
                    let names: Vec<_> = body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .inspect(|tool| assert!(tool.get("strict").is_none()))
                        .map(|tool| tool["name"].as_str().unwrap().to_owned())
                        .collect();
                    for expected in ["arany_read", "arany_edit", "arany_command"] {
                        assert!(names.iter().any(|name| name == expected), "{names:?}");
                    }
                }
                assert_eq!(observations.len(), step);
                if step == 1 {
                    assert_eq!(observations[0]["disposition"], "succeeded");
                    assert_eq!(observations[0]["workspace_effect"], "none");
                    assert!(
                        observations[0].get("intent").is_none()
                            && observations[0].get("attestation").is_none()
                    );
                    assert_eq!(
                        observations[0]["output"],
                        json!({"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"native command passed","stderr":"","stdout_total":{"bytes":21,"lines":1},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}})
                    );
                }
                assert_eq!(context["tools"]["catalog"]["write"], true);
                let outcome = match step {
                    0 => {
                        json!({"outcome":{"type":"tool","call":{"operation":"command","program":"bash","args":["src/check.sh"],"cwd":""}}})
                    }
                    1 => {
                        json!({"outcome":{"type":"tool","call":{"operation":"read","path":"src/note.txt","offset":null,"limit":null}}})
                    }
                    2 => {
                        assert!(observations[1].get("call").is_none());
                        assert_eq!(observations[1]["workspace_effect"], "none");
                        assert_eq!(observations[1]["output"]["text"], "1\tnative note\n");
                        assert_eq!(observations[1]["output"]["sha256"], hash(b"native note\n"));
                        json!({"outcome":{"type":"tool","call":{"operation":"edit","path":"src/note.txt","expected_digest":observations[1]["output"]["sha256"],"old":"native note\n","new":"native note\n2026-10-06\n"}}})
                    }
                    3 => {
                        assert!(observations[2].get("call").is_none());
                        assert_eq!(observations[2]["disposition"], "succeeded");
                        assert_eq!(observations[2]["workspace_effect"], "applied");
                        assert_eq!(
                            observations[2]["output"]["sha256"],
                            hash(b"native note\n2026-10-06\n")
                        );
                        json!("Native command and edit passed.")
                    }
                    _ => unreachable!(),
                };
                contexts.push(context);
                let response = if outcome.is_string() {
                    if profile == "openai" {
                        assert_eq!(body["tool_choice"]["mode"], "auto");
                        json!({"id":format!("resp_native_{step}"), "status":"completed", "model":model, "output":arany::semantic::responses_output(&outcome), "usage":{"input_tokens":12,"output_tokens":8}})
                    } else {
                        json!({"id":format!("msg_native_{step}"),"type":"message","role":"assistant","model":model,"stop_reason":"end_turn","content":arany::semantic::messages_content(&outcome),"usage":{"input_tokens":12,"output_tokens":8}})
                    }
                } else {
                    let tool = outcome["outcome"]["type"] == "tool";
                    let mut arguments = if tool {
                        outcome["outcome"]["call"].clone()
                    } else {
                        outcome["outcome"].clone()
                    };
                    let key = if tool { "operation" } else { "type" };
                    let operation = arguments.as_object_mut().unwrap().remove(key).unwrap();
                    if profile == "openai" {
                        assert_eq!(body["tool_choice"]["mode"], "auto");
                        assert_eq!(body["parallel_tool_calls"], true);
                        json!({"id":format!("resp_native_{step}"),"status":"completed","model":model,"output":[
                            {"type":"reasoning","id":format!("rs_native_{step}"),"summary":[],"encrypted_content":"CIPHERTEXT=="},
                            {"type":"function_call","id":format!("fc_native_{step}"),"call_id":format!("call_native_{step}"),"name":format!("arany_{}", operation.as_str().unwrap()),"arguments":arguments.to_string(),"status":"completed"}
                        ],"usage":{"input_tokens":12,"output_tokens":8}})
                    } else if tool {
                        json!({"id":format!("msg_native_{step}"),"type":"message","role":"assistant","model":model,"stop_reason":"tool_use","stop_sequence":null,"content":[
                            {"type":"thinking","thinking":"private","signature":"sig"},
                            {"type":"tool_use","id":format!("toolu_native_{step}"),"name":format!("arany_{}", operation.as_str().unwrap()),"input":arguments}
                        ],"usage":{"input_tokens":12,"output_tokens":8}})
                    } else {
                        json!({"id":format!("msg_native_{step}"),"type":"message","role":"assistant","model":model,"stop_reason":"end_turn","stop_sequence":null,"content": arany::semantic::messages_content(&outcome),"usage":{"input_tokens":12,"output_tokens":8}})
                    }
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
        let response = responder.join();
        if !output.status.success() || response.is_err() {
            let failure = if output.status.success() {
                "provider_exchange"
            } else {
                "product_exit"
            };
            let profile = if profile == "openai" {
                "openai"
            } else {
                "anthropic"
            };
            crate::process::annotate_native_fixture(&format!(
                "fixture=shipped_exec; profile={profile}; failure={failure}; {}",
                exec_failure_summary(&output, response.is_ok())
            ));
        }
        assert!(
            output.status.success(),
            "native exec failed: {}",
            exec_failure_summary(&output, response.is_ok())
        );
        let contexts = response.unwrap_or_else(|_| {
            panic!(
                "strict native request semantics failed: {}",
                exec_failure_summary(&output, false)
            )
        });
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
            let replay = SessionView::replay(session_id, &events).unwrap().unwrap();
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
            assert_eq!(replay.runs, vec![run.clone()]);
            assert_eq!(replay.goals.len(), 1);
            let goal = &replay.goals[0];
            assert_eq!(goal.status, arany::GoalStatus::Completed);
            assert!(goal.plan_contract.is_none());
            assert!(goal.success_criterion_satisfied);
            assert!(!goal.awaiting_plan_contract);
            assert!(goal.report.is_some());
            assert_eq!(goal.run_ids, vec![run.id]);
            assert_eq!(
                output.stdout,
                arany_cli::presentation::render_exec(
                    &run,
                    &events,
                    arany_cli::presentation::Output::Jsonl,
                    true
                )
                .into_bytes()
            );
            store.close().await.unwrap();
        });
    }
}

#[test]
#[ignore = "requires an explicit local Rust sysroot; native offline Guard Cargo journey"]
fn guarded_offline_cargo_tests_use_selected_runtime_and_registry() {
    use std::os::unix::fs::DirBuilderExt;
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = std::env::var_os("ARANY_TEST_RUST_SYSROOT")
        .expect("explicit physical versioned Rust sysroot");
    let source = Path::new(&source);
    assert!(source.is_absolute());
    assert_eq!(std::fs::canonicalize(source).unwrap(), source);
    let temp = tempfile::tempdir().unwrap();
    for name in ["account-home", "home", "workspace"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(temp.path().join(name))
            .unwrap();
    }
    let toolchains = temp.path().join("home/.rustup/toolchains");
    std::fs::create_dir_all(&toolchains).unwrap();
    let copied = Command::new("/usr/bin/cp")
        .args(["--reflink=auto", "-R", "--"])
        .arg(source)
        .arg(&toolchains)
        .env_clear()
        .current_dir(temp.path())
        .bounded_output_for(Duration::from_secs(60), 4096)
        .unwrap();
    assert!(copied.status.success(), "private Rust clone: {copied:?}");
    let mut helper = crate::isolation::native_account_isolated_test_command_with_timeout(
        temp.path(),
        &std::env::current_exe().unwrap(),
        Path::new(env!("CARGO_BIN_EXE_arany")),
        1100,
    );
    helper
        .args([
            "--exact",
            "tools::native_exec::offline_cargo_helper",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .current_dir(temp.path());
    let output = helper
        .bounded_output_for(Duration::from_secs(1120), 64 * 1024)
        .unwrap();
    if !output.status.success() {
        let retained = temp.keep();
        panic!("offline Cargo journey failed; fixture={retained:?}; output={output:?}");
    }
    crate::process::assert_single_test_completed(&output.stdout, "offline Cargo journey");
}

#[tokio::test]
#[ignore = "isolated helper owned by native offline Cargo journey"]
async fn offline_cargo_helper() {
    let root = std::env::var_os("ARANY_TEST_CACHE_CANARY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    let workspace = root.join("workspace");
    arany::test_support::scenario::offline_cargo_fixture(&root.join("home"), &workspace);
    let mut permissions = arany::WorkspacePermissions::open(&workspace)
        .unwrap()
        .prepare_profile_selection();
    let choices = permissions.runtime_profile_choices();
    let rust = choices
        .iter()
        .find(|choice| choice.group == "Rust")
        .expect("one explicit private Rust toolchain");
    assert!(rust.selected);
    permissions
        .remember_profiles(
            true,
            arany::ApprovalMode::Request,
            std::slice::from_ref(&rust.token),
        )
        .unwrap();
    arany::test_support::scenario::guarded_offline_cargo_journey(
        root,
        &workspace,
        Path::new(env!("CARGO_BIN_EXE_arany")),
    )
    .await;
}

pub(super) struct SpillJourney {
    pub(super) script: arany::test_support::scenario::ScriptedProvider,
    pub(super) command: ToolCall,
}

#[async_trait::async_trait]
impl Provider for SpillJourney {
    fn identity(&self) -> &arany::ProviderIdentity {
        self.script.identity()
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let observations = request
            .input
            .tools
            .as_ref()
            .map(|tools| tools.observations())
            .unwrap_or_default();
        let outcome = match observations.as_slice() {
            [] if !crate::planned(&request) => arany::test_support::scenario::plan_outcome(
                proposal_plan_contract(&self.command).ok_or(ProviderError::InvalidOutcome)?,
            ),
            [] => crate::tool_outcome(self.command.clone()),
            [command] => {
                let spill = command
                    .spill
                    .as_ref()
                    .ok_or(ProviderError::InvalidOutcome)?;
                crate::tool_outcome(ToolCall::Read {
                    path: format!("spill:{}", spill.r#ref),
                    offset: Some(2),
                    limit: Some(1),
                })
            }
            [_, _] => crate::tool_outcome(ToolCall::Read {
                path: "src/current.txt".into(),
                offset: None,
                limit: None,
            }),
            [_, _, _] => ProviderOutcome::Answer(Answer {
                text: "Paged retained output".into(),
            }),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        self.script.responses.lock().unwrap().push_back(Ok(outcome));
        self.script.invoke(request).await
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.script.compact(request).await
    }
}

struct SpillFixture {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    state: PathBuf,
    engine: arany::Engine,
    command: ToolCall,
}

impl SpillFixture {
    fn set_workspace_root(&self, root: &str) {
        let path = self.state.join("tools.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["workspace_paths"] = json!([root]);
        private_file(&path, &serde_json::to_vec(&config).unwrap());
    }

    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        private_file(&workspace.join("src/current.txt"), b"current");
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        let shell = std::fs::canonicalize("/usr/bin/bash").unwrap();
        private_file(&state_path.join("tools.json"), &serde_json::to_vec(&json!({"version":1,"workspace_paths":["src"],"write":false,"commands":[{"name":"bash","executable":shell,"sha256":hash(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]})).unwrap());
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let command = ToolCall::Command {
            program: "bash".into(),
            args: vec![
                "-c".into(),
                "printf '%1572864s' ''; printf '\nPRIVATE_SPILL_CANARY\n'; printf '%1572864s' ''"
                    .into(),
            ],
            cwd: "".into(),
        };
        Self {
            _temp: temp,
            workspace,
            state: state_path,
            engine,
            command,
        }
    }
}

const SPILL_JOURNEY_WAIT: Duration = Duration::from_secs(120);
const SPILL_SETTLE_WAIT: Duration = Duration::from_secs(10);

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux output capture and private spill paging"
)]
fn command_spill_pages_private_output_and_survives_reopen() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline("spill paging", SPILL_JOURNEY_WAIT);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(spill_read_journey());
}

async fn spill_read_journey() {
    let SpillFixture {
        _temp,
        workspace,
        state: state_path,
        engine,
        command,
    } = SpillFixture::new();
    let provider = Arc::new(SpillJourney {
        script: arany::test_support::scenario::ScriptedProvider::new(vec![]),
        command,
    });
    let outcome = run_with(
        &engine,
        &provider,
        proposal_request(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(provider.script.requests.lock().unwrap().len(), 5);
    let command = outcome.run.tools[0].observation.as_ref().unwrap();
    let spill = command.spill.as_ref().unwrap();
    assert!(!spill.truncated);
    assert!(!command.output.contains("PRIVATE_SPILL_CANARY"));
    let read = outcome.run.tools[1].observation.as_ref().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&read.output).unwrap()["text"],
        "2\tPRIVATE_SPILL_CANARY\n"
    );
    assert!(matches!(
        read.attestation,
        Some(arany::Attestation::InProcess { .. })
    ));
    let path = state_path
        .join("spill")
        .join(outcome.run.id.to_string())
        .join(format!("{}.txt", spill.r#ref));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), spill.bytes);
    engine.close().await.unwrap();
    let orphan = path
        .parent()
        .unwrap()
        .join(format!("{}.txt", "f".repeat(64)));
    private_file(&orphan, b"UNCOMMITTED_SPILL_CANARY");
    let (events, replay) =
        arany::test_support::goals::replayed(&state_path, outcome.session_id).await;
    assert_eq!(replay.runs[0], outcome.run);
    assert_eq!(replay.goals[0].status, arany::GoalStatus::Completed);
    let text = arany_cli::presentation::render_session(
        &replay,
        &events,
        arany_cli::presentation::Output::Text,
    );
    assert!(text.contains(&format!("spill:{}", spill.r#ref)));
    assert!(
        events
            .iter()
            .all(|event| { !event.event.payload().unwrap().contains("\"capture\"") })
    );
    let engine = arany::test_support::engine::open(
        StateRoot::open_existing(&state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!orphan.exists());
    assert_spill_session_scope(&engine, &workspace, outcome.session_id, &spill.r#ref).await;
    engine.close().await.unwrap();
}

async fn assert_spill_session_scope(
    engine: &arany::Engine,
    workspace: &Path,
    session: arany::SessionId,
    reference: &str,
) {
    for same_session in [true, false] {
        let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
            crate::tool_outcome(ToolCall::Read {
                path: format!("spill:{}", reference),
                offset: Some(2),
                limit: Some(1),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Read checked".into(),
            }),
        ]));
        let mut request = proposal_request(workspace);
        request.session_id = same_session.then_some(session);
        let result = run_with(
            engine,
            &provider,
            request,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let read = result.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(
            read.disposition,
            if same_session {
                ToolDisposition::Succeeded
            } else {
                ToolDisposition::Failed
            }
        );
        if same_session {
            assert_eq!(
                serde_json::from_str::<Value>(&read.output).unwrap()["text"],
                "2\tPRIVATE_SPILL_CANARY\n"
            );
        } else {
            assert_eq!(read.output, "no spill for this Run");
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum StallEnd {
    Cancel,
    Deadline,
}

impl StallEnd {
    fn expected_outcome(self) -> (RunStatus, usize) {
        match self {
            Self::Cancel => (RunStatus::Cancelled, 2),
            Self::Deadline => (RunStatus::Finished, 4),
        }
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux spill persistence cancellation and deadline"
)]
fn spill_persistence_stall_preserves_completed_effects_and_reopens() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = arany::test_support::engine::deadline("spill persistence", SPILL_JOURNEY_WAIT);
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for end in [StallEnd::Cancel, StallEnd::Deadline] {
        reactor.block_on(spill_persistence_stall(end));
    }
}

async fn spill_persistence_stall(end: StallEnd) {
    let SpillFixture {
        _temp,
        workspace,
        state,
        engine,
        command,
    } = SpillFixture::new();
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(proposal_plan_contract(&command).unwrap()),
        crate::tool_outcome(command),
        crate::tool_outcome(ToolCall::Read {
            path: "src/current.txt".into(),
            offset: None,
            limit: None,
        }),
        ProviderOutcome::Answer(Answer {
            text: "Completed without spill".into(),
        }),
    ]));
    let (mut entered, release, finished) = arany::FaultPlan::gate_spill_write(state.clone());
    let cancellation = arany::RunCancellation::new();
    let outcome = {
        let run = run_with(
            &engine,
            &provider,
            proposal_request(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                cancellation: cancellation.clone(),
                ..Default::default()
            },
        );
        tokio::pin!(run);
        tokio::select! {
            entered = tokio::time::timeout(SPILL_JOURNEY_WAIT, &mut entered) => entered.unwrap().unwrap(),
            outcome = &mut run => panic!("Run settled before spill persistence gate: {:?}", outcome.map(|result| result.run.status)),
        }
        match end {
            StallEnd::Cancel => {
                cancellation.cancel();
            }
            StallEnd::Deadline => {
                let timeout = arany::limits::LIMITS
                    .rows
                    .iter()
                    .find(|limit| limit.name == "SPILL_PERSIST_TIMEOUT_MS")
                    .unwrap()
                    .value;
                tokio::time::pause();
                tokio::time::advance(Duration::from_millis(timeout)).await;
                tokio::time::resume();
            }
        }
        tokio::time::timeout(SPILL_SETTLE_WAIT, &mut run)
            .await
            .expect("spill persistence cannot hold the Run")
            .unwrap()
    };
    let (expected_status, expected_requests) = end.expected_outcome();
    assert_eq!(outcome.run.status, expected_status, "{end:?}");
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        expected_requests,
        "{end:?}"
    );
    assert_excerpt_only_success(&outcome.run);
    engine.close().await.unwrap();
    assert_spill_recovery(&state, &outcome, release, finished).await;
}

async fn assert_spill_recovery(
    state: &Path,
    outcome: &arany::RunOutcome,
    release: std::sync::mpsc::SyncSender<()>,
    finished: tokio::sync::oneshot::Receiver<()>,
) {
    let digest = Sha256::digest(serde_json::to_vec(&outcome.run.tools[0].intent).unwrap());
    let reference = hash(&digest);
    let path = state
        .join("spill")
        .join(outcome.run.id.to_string())
        .join(format!("{reference}.txt"));
    assert!(
        path.exists(),
        "stalled spill must exist before reopening the Store"
    );
    let store = Store::open(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs[0], outcome.run);
    assert!(
        path.exists(),
        "open must skip a writer that still owns its lock"
    );
    store.close().await.unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(SPILL_SETTLE_WAIT, finished)
        .await
        .unwrap()
        .unwrap();
    let store = Store::open(StateRoot::open_existing(state).unwrap()).unwrap();
    assert!(
        !path.exists(),
        "late persistence cannot publish an uncommitted spill"
    );
    let replay = store.load_view(outcome.session_id).await.unwrap().unwrap();
    assert_eq!(replay.runs[0], outcome.run);
    assert_excerpt_only_success(&replay.runs[0]);
    store.close().await.unwrap();
}

fn assert_excerpt_only_success(run: &arany::RunView) {
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Guard(_))
    ));
    assert!(observation.spill.is_none());
    let result: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(result["exit_code"], 0);
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains(" lines omitted; ")
    );
    assert!(result["stdout_total"]["bytes"].as_u64().unwrap() > observation.output.len() as u64);
}

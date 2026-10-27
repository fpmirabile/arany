#[cfg(any(debug_assertions, arany_ci))]
use crate::engine_support::run_with;
#[cfg(any(debug_assertions, arany_ci))]
use crate::git::{host_git, tree_snapshot};
#[cfg(any(debug_assertions, arany_ci))]
use crate::{hash, private_file};
#[cfg(any(debug_assertions, arany_ci))]
use arany::{
    Answer, CollaborationPolicy, CompactionRequest, CompactionResponse, Provider, ProviderError,
    ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus, SessionView,
    StateRoot, Store, ToolCall, ToolDisposition,
};
#[cfg(any(debug_assertions, arany_ci))]
use arany::{EngineConfig, RunControls, ToolGrant};
#[cfg(any(debug_assertions, arany_ci))]
use serde_json::json;
#[cfg(any(debug_assertions, arany_ci))]
use std::path::Path;
#[cfg(any(debug_assertions, arany_ci))]
use std::sync::{Arc, Mutex};
#[cfg(any(debug_assertions, arany_ci))]
use std::time::Duration;

#[cfg(any(debug_assertions, arany_ci))]
const CLT_GIT: &str = "/Library/Developer/CommandLineTools/usr/bin/git";

#[cfg(any(debug_assertions, arany_ci))]
struct GitJourney {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[cfg(any(debug_assertions, arany_ci))]
#[async_trait::async_trait]
impl Provider for GitJourney {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "tool-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let step = requests.len();
        requests.push(request);
        let outcome = match step {
            0 => crate::tool_outcome(ToolCall::GitStatus { path: ".".into() }),
            1 => crate::tool_outcome(ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            }),
            2 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 5,
            }),
            3 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing-branch".into()),
                max_count: 1,
            }),
            4 => crate::tool_outcome(ToolCall::GitBranch {
                name: None,
                start_point: None,
                scope: Some("all".into()),
                base: Some("HEAD~1".into()),
                max_count: Some(16),
            }),
            5 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("HEAD~1..protected".into()),
                max_count: 5,
            }),
            6 => crate::tool_outcome(ToolCall::GitDiff {
                path: ".".into(),
                base: Some("HEAD~1".into()),
                head: Some("HEAD".into()),
                staged: false,
            }),
            7 => crate::tool_outcome(ToolCall::GitLog {
                path: "denied/history.txt".into(),
                revision: Some("HEAD~1..protected".into()),
                max_count: 5,
            }),
            8 => crate::tool_outcome(ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing-endpoint..HEAD".into()),
                max_count: 5,
            }),
            9 => ProviderOutcome::Answer(Answer {
                text: "Git state observed".into(),
            }),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        Ok(ProviderResponse {
            outcome,
            turn: arany::ProviderTurn::scripted(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        _request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS Git journey; release requires CI's disposable account"
)]
fn guarded_native_git_reads_survive_closed_replay() {
    #[cfg(debug_assertions)]
    crate::tools::native_journey("tools_git::native_git_journey_helper");
    #[cfg(all(not(debug_assertions), arany_ci))]
    release_git_journey();
    #[cfg(all(not(debug_assertions), not(arany_ci)))]
    panic!("release Git journey requires CI's disposable account");
}

#[cfg(all(not(debug_assertions), arany_ci))]
fn release_git_journey() {
    use crate::process::BoundedOutput;

    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    for directory in ["home", "environment-state", "environment-data"] {
        std::fs::create_dir(temp.path().join(directory)).unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tools_git::native_git_journey_helper",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("ARANY_TEST_FILE_JOURNEY_ROOT", temp.path())
        .env("HOME", temp.path().join("home"))
        .env("XDG_STATE_HOME", temp.path().join("environment-state"))
        .env("XDG_DATA_HOME", temp.path().join("environment-data"))
        .current_dir(temp.path());
    crate::process::isolate_host_authentication(&mut command, &temp.path().join("home"));
    let output = command
        .bounded_output_for(Duration::from_secs(360), 32 * 1024)
        .unwrap();
    assert!(
        output.status.success(),
        "native Git journey: {:?}; stdout={:?}; stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    crate::process::assert_single_test_completed(&output.stdout, "native Git journey");
    assert!(output.stderr.is_empty());
}

#[cfg(any(debug_assertions, arany_ci))]
fn unit_leftovers() -> Vec<String> {
    const PARENT: &str = "/private/tmp";
    const PREFIX: &str = "arany-u-";
    let mut found: Vec<String> = std::fs::read_dir(PARENT)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(PREFIX))
        .collect();
    use crate::process::BoundedOutput;
    let mounts = std::process::Command::new("/sbin/mount")
        .bounded_output_for(Duration::from_secs(10), 1024 * 1024)
        .unwrap();
    assert!(mounts.status.success(), "mount probe: {mounts:?}");
    found.extend(
        String::from_utf8_lossy(&mounts.stdout)
            .lines()
            .filter(|line| line.contains(&format!("{PARENT}/{PREFIX}")))
            .map(str::to_owned),
    );
    found.sort();
    found
}

#[cfg(any(debug_assertions, arany_ci))]
async fn faulted_git_read(
    engine: &arany::Engine,
    workspace: &Path,
    fault: arany::GitStorageFault,
    refusal_evidence: &Path,
) -> arany::RunOutcome {
    let call = ToolCall::GitStatus { path: ".".into() };
    assert!(!refusal_evidence.exists());
    arany::FaultPlan::fail_git_storage(call.clone(), fault, refusal_evidence.into());
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(call),
        arany::test_support::goals::finish_with("Done"),
    ]));
    run_with(
        engine,
        &provider,
        RunRequest {
            skill: None,
            session_id: None,
            title: Some(format!("{fault:?} Git")),
            objective: "Fault this Git unit".into(),
            images: Vec::new(),
            workspace: workspace.into(),
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
    .unwrap()
}

#[cfg(any(debug_assertions, arany_ci))]
async fn cancelled_git_read(engine: &arany::Engine, workspace: &Path) -> arany::SessionId {
    let call = ToolCall::GitStatus { path: ".".into() };
    let session = engine
        .create_session(workspace.into(), Some("Cancelled Git".into()))
        .await
        .unwrap();
    let (dispatched, release) = arany::FaultPlan::gate_guard_dispatch(call.clone());
    let cancellation = arany::RunCancellation::new();
    let pending = cancellation.clone();
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(call),
    ]));
    let stop = async {
        dispatched.await.unwrap();
        assert!(pending.cancel());
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            run_with(
                engine,
                &provider,
                RunRequest {
                    skill: None,
                    session_id: Some(session),
                    title: None,
                    objective: "Cancel this Git read".into(),
                    images: Vec::new(),
                    workspace: workspace.into(),
                    launch_directory: None,
                    include_paths: Vec::new(),
                    policy: CollaborationPolicy::Single,
                    goal: arany::GoalRequest::default(),
                },
                RunControls {
                    tools: ToolGrant::Configured,
                    cancellation,
                    ..Default::default()
                },
            ),
            stop
        )
    })
    .await
    .expect("cancelled Git read deadline");
    drop(release);
    assert!(
        matches!(
            &outcome,
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
        ),
        "dispatched Git cancellation must preserve uncertainty: {outcome:?}"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    session
}

#[cfg(any(debug_assertions, arany_ci))]
fn assert_cancelled_git_read(session: arany::SessionId, events: &[arany::EventEnvelope]) {
    let replay = SessionView::replay(session, events).unwrap().unwrap();
    assert_eq!(replay.runs.len(), 1);
    let run = &replay.runs[0];
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.tools.len(), 1);
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert!(matches!(
        &observation.intent.call,
        ToolCall::GitStatus { .. }
    ));
    assert_eq!(observation.disposition, ToolDisposition::Uncertain);
    assert!(observation.attestation.is_none());
    assert_eq!(
        observation.output,
        "Guard execution unconfirmed; effects may have occurred. Do not retry automatically."
    );
    assert_eq!(replay.goals.len(), 1);
    assert_eq!(replay.goals[0].status, arany::GoalStatus::Interrupted);
}

#[cfg(any(debug_assertions, arany_ci))]
#[tokio::test]
#[ignore = "isolated helper owned by native Git journey"]
async fn native_git_journey_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    #[cfg(debug_assertions)]
    {
        crate::tools::trace_case(root, "git");
        assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
    }
    #[cfg(not(debug_assertions))]
    assert!(std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_none());
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(workspace.join("cfg")).unwrap();
    host_git(CLT_GIT, &workspace, &home, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
    std::fs::write(workspace.join(".gitignore"), "cfg/private.json\n").unwrap();
    std::fs::write(
        workspace.join("cfg/private.json"),
        "GIT_DISCLOSURE_CANARY=initial\n",
    )
    .unwrap();
    host_git(CLT_GIT, &workspace, &home, &["add", "-A"]);
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["add", "-f", "cfg/private.json"],
    );
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "first commit"],
    );
    std::fs::write(
        workspace.join("cfg/private.json"),
        "GIT_DISCLOSURE_CANARY=committed\n",
    )
    .unwrap();
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["add", "-f", "cfg/private.json"],
    );
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "GIT_DISCLOSURE_CANARY history"],
    );
    std::fs::write(workspace.join("src/a.txt"), "bravo\n").unwrap();
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-am", "second commit"],
    );
    std::fs::write(workspace.join(".env"), "private fixture\n").unwrap();
    host_git(CLT_GIT, &workspace, &home, &["add", ".env"]);
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "BRANCH_DISCLOSURE_CANARY"],
    );
    std::fs::write(
        workspace.join("cfg/private.json"),
        "GIT_DISCLOSURE_CANARY=working\n",
    )
    .unwrap();
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["update-ref", "refs/heads/protected", "HEAD"],
    );
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["update-ref", "refs/heads/main", "HEAD~1"],
    );
    std::fs::create_dir_all(workspace.join(".git/refs/remotes/review")).unwrap();
    std::fs::copy(
        workspace.join(".git/refs/heads/main"),
        workspace.join(".git/refs/remotes/review/topic"),
    )
    .unwrap();
    std::fs::write(workspace.join("src/a.txt"), "delta\n").unwrap();
    std::fs::write(workspace.join("src/new.txt"), "new\n").unwrap();
    {
        use std::io::Write;
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap();
        config
            .write_all(
                b"[core]\n\tfsmonitor = /usr/bin/false\n[diff]\n\texternal = /usr/bin/false\n[filter \"evil\"]\n\tclean = /usr/bin/false\n\trequired = true\n",
            )
            .unwrap();
    }
    std::fs::write(workspace.join(".gitattributes"), "* filter=evil\n").unwrap();
    let before = tree_snapshot(&workspace.join(".git"));

    let state_path = root.join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(
        &state_path.join("permissions.json"),
        br#"{"permissions":{"deny":["Read(./denied/**)"]}}"#,
    );
    let git = std::fs::canonicalize(CLT_GIT).unwrap();
    let config = json!({
        "version": 1,
        "workspace_paths": ["."],
        "write": false,
        "commands": [],
        "skills": [],
        "mcp": [],
        "git": {
            "name": "git",
            "executable": git,
            "sha256": hash(&std::fs::read(&git).unwrap()),
            "interpreter": false,
            "inputs": []
        }
    });
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(GitJourney {
        requests: requests.clone(),
    });
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let leftovers = unit_leftovers();
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        run_with(
            &engine,
            &provider,
            RunRequest {
                skill: None,
                session_id: None,
                title: Some("Git".into()),
                objective: "Inspect Git state".into(),
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
        ),
    )
    .await
    .expect("Git journey deadline")
    .unwrap();
    let cancelled = cancelled_git_read(&engine, &workspace).await;
    for fault in [
        arany::GitStorageFault::Refuse,
        arany::GitStorageFault::Exhaust,
    ] {
        let refusal_evidence = root.join(format!("git-storage-{fault:?}.json"));
        let faulted = faulted_git_read(&engine, &workspace, fault, &refusal_evidence).await;
        assert_eq!(faulted.run.tools.len(), 1, "{fault:?}");
        let observation = faulted.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            ToolDisposition::Failed,
            "{fault:?}"
        );
        match fault {
            arany::GitStorageFault::Refuse => {
                assert!(observation.attestation.is_none(), "{observation:?}");
                let (intent, unit): (uuid::Uuid, std::path::PathBuf) = serde_json::from_slice(
                    &std::fs::read(&refusal_evidence)
                        .expect("Git unit attached its volume, checked the canary and refused"),
                )
                .unwrap();
                assert_eq!(intent, observation.intent.id);
                assert!(!unit.exists(), "refused Git unit root must be removed");
            }
            arany::GitStorageFault::Exhaust => {
                assert!(observation.attestation.is_some(), "{observation:?}");
                assert_eq!(
                    observation.output,
                    "Tool operation failed; no automatic retry"
                );
            }
        }
    }
    engine.close().await.unwrap();
    assert_eq!(
        unit_leftovers(),
        leftovers,
        "every Git unit, including the failed and denied reads, must detach its volume and remove its root"
    );
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{:?}",
        outcome.run.tools
    );
    assert_eq!(outcome.run.tools.len(), 9);

    let output = |index: usize| -> serde_json::Value {
        let observation = outcome.run.tools[index].observation.as_ref().unwrap();
        serde_json::from_str(&observation.output).unwrap()
    };
    for index in 0..3 {
        let observation = outcome.run.tools[index].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.attestation.is_some());
        assert!(
            output(index).get("failed").is_none(),
            "{}",
            observation.output
        );
    }
    let status = output(0);
    let entries = status["entries"].as_array().unwrap();
    assert!(entries.contains(&json!({"path": "src/a.txt", "state": ".M"})));
    assert!(entries.contains(&json!({"path": "src/new.txt", "state": "??"})));
    assert!(
        !outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .output
            .contains("GIT_DISCLOSURE_CANARY")
    );
    assert!(
        !outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .output
            .contains("cfg/private.json")
    );
    let diff = output(1);
    assert_eq!(
        diff["files"],
        json!([{"path": "src/a.txt", "added": 1, "deleted": 1}])
    );
    let patch = diff["patch"].as_str().unwrap();
    assert!(patch.contains("-bravo") && patch.contains("+delta"));
    assert!(
        !outcome.run.tools[1]
            .observation
            .as_ref()
            .unwrap()
            .output
            .contains("GIT_DISCLOSURE_CANARY")
    );
    assert!(
        !outcome.run.tools[1]
            .observation
            .as_ref()
            .unwrap()
            .output
            .contains("cfg/private.json")
    );
    let log = output(2);
    let subjects: Vec<&str> = log["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["second commit", "first commit"]);
    assert!(
        !outcome.run.tools[2]
            .observation
            .as_ref()
            .unwrap()
            .output
            .contains("GIT_DISCLOSURE_CANARY")
    );
    let failed = outcome.run.tools[3].observation.as_ref().unwrap();
    assert_eq!(failed.disposition, ToolDisposition::Failed);
    assert!(failed.attestation.is_some());

    let branches = output(4);
    crate::assert_restricted_git_branch_journey(&branches, &workspace);
    let observation = outcome.run.tools[4].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(observation.attestation.is_some());

    crate::assert_git_range_outputs(&outcome.run, &workspace, &home, CLT_GIT);
    let denied = outcome.run.tools[7].observation.as_ref().unwrap();
    assert_eq!(denied.disposition, ToolDisposition::Denied);
    assert!(!denied.output.contains("BRANCH_DISCLOSURE_CANARY"));
    let missing_endpoint = outcome.run.tools[8].observation.as_ref().unwrap();
    assert_eq!(missing_endpoint.disposition, ToolDisposition::Failed);

    let requests = requests.lock().unwrap().clone();
    assert!(!format!("{requests:?}").contains("BRANCH_DISCLOSURE_CANARY"));
    assert!(!format!("{requests:?}").contains("GIT_DISCLOSURE_CANARY"));
    assert_eq!(
        requests[5]
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations()
            .last()
            .unwrap()
            .output,
        observation.output
    );
    crate::assert_git_range_continuation(&outcome.run, &requests);
    let catalog: serde_json::Value =
        serde_json::from_str(requests[0].input.tools.as_ref().unwrap().catalog()).unwrap();
    assert_eq!(
        catalog["git"]["operations"],
        json!(["git_status", "git_diff", "git_log", "git_branch"])
    );
    let after = tree_snapshot(&workspace.join(".git"));
    let changed: Vec<_> = before
        .iter()
        .chain(&after)
        .filter(|entry| !(before.contains(entry) && after.contains(entry)))
        .map(|entry| &entry.0)
        .collect();
    assert!(changed.is_empty(), "repository changed: {changed:?}");

    std::fs::rename(workspace.join(".git"), home.join("repository-after-run")).unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let cancelled_events = store.load_session(cancelled).await.unwrap();
    assert_cancelled_git_read(cancelled, &cancelled_events);
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.last().unwrap(), &outcome.run);
    crate::assert_git_range_replay(&outcome.run, replay.runs.last().unwrap());
    store.close().await.unwrap();
    crate::assert_closed_git_views(&state_path, &home, &workspace, &events, &replay);
    let is_git = |call: &ToolCall| {
        matches!(
            call,
            ToolCall::GitStatus { .. }
                | ToolCall::GitDiff { .. }
                | ToolCall::GitLog { .. }
                | ToolCall::GitBranch { name: None, .. }
        )
    };
    let started = events
        .iter()
        .filter(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if is_git(&intent.call)))
        .count();
    let finished = events
        .iter()
        .filter(|event| matches!(&event.event, arany::Event::ToolFinished { observation, .. } if is_git(&observation.intent.call)))
        .count();
    assert_eq!((started, finished), (9, 9));
}

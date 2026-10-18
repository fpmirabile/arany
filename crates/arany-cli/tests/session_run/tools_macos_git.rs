#[cfg(any(debug_assertions, arany_ci))]
use crate::engine_support::run_with;
#[cfg(any(debug_assertions, arany_ci))]
use crate::git::{host_git, tree_snapshot};
#[cfg(any(debug_assertions, arany_ci))]
use crate::{hash, private_file};
#[cfg(any(debug_assertions, arany_ci))]
use arany::{
    CollaborationPolicy, CompactionRequest, CompactionResponse, Finish, Provider, ProviderError,
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
                arany::ProviderIdentity::new("scripted", "tool-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let step = requests.len();
        requests.push(request);
        let outcome = match step {
            0 => crate::tool_outcome(ToolCall::GitStatus { path: ".".into() }, None),
            1 => crate::tool_outcome(
                ToolCall::GitDiff {
                    path: "src".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                None,
            ),
            2 => crate::tool_outcome(
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 5,
                },
                None,
            ),
            3 => crate::tool_outcome(
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: Some("missing-branch".into()),
                    max_count: 1,
                },
                None,
            ),
            4 => ProviderOutcome::Finish(Finish {
                summary: "Inspected".into(),
                result: "Git state observed".into(),
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
    let output = std::process::Command::new(std::env::current_exe().unwrap())
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
        .current_dir(temp.path())
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
    host_git(CLT_GIT, &workspace, &home, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
    host_git(CLT_GIT, &workspace, &home, &["add", "-A"]);
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "first commit"],
    );
    std::fs::write(workspace.join("src/a.txt"), "bravo\n").unwrap();
    host_git(
        CLT_GIT,
        &workspace,
        &home,
        &["commit", "-q", "-am", "second commit"],
    );
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
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        run_with(
            &engine,
            &provider,
            RunRequest {
                session_id: None,
                title: Some("Git".into()),
                objective: "Inspect Git state".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
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
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{:?}",
        outcome.run.tools
    );
    assert_eq!(outcome.run.tools.len(), 4);

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
    let diff = output(1);
    assert_eq!(
        diff["files"],
        json!([{"path": "src/a.txt", "added": 1, "deleted": 1}])
    );
    let patch = diff["patch"].as_str().unwrap();
    assert!(patch.contains("-bravo") && patch.contains("+delta"));
    let log = output(2);
    let subjects: Vec<&str> = log["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["second commit", "first commit"]);
    let failed = outcome.run.tools[3].observation.as_ref().unwrap();
    assert_eq!(failed.disposition, ToolDisposition::Failed);
    assert!(failed.attestation.is_some());

    let requests = requests.lock().unwrap().clone();
    let catalog: serde_json::Value =
        serde_json::from_str(&requests[0].input.tools.as_ref().unwrap().catalog).unwrap();
    assert_eq!(
        catalog["git"]["operations"],
        json!(["git_status", "git_diff", "git_log"])
    );
    let after = tree_snapshot(&workspace.join(".git"));
    let changed: Vec<_> = before
        .iter()
        .chain(&after)
        .filter(|entry| !(before.contains(entry) && after.contains(entry)))
        .map(|entry| &entry.0)
        .collect();
    assert!(changed.is_empty(), "repository changed: {changed:?}");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs.last().unwrap(), &outcome.run);
    store.close().await.unwrap();
    let is_git = |call: &ToolCall| {
        matches!(
            call,
            ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } | ToolCall::GitLog { .. }
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
    assert_eq!((started, finished), (4, 4));
}

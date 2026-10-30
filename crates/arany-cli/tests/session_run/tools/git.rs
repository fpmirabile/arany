use super::*;
use crate::git::{host_git, tree_snapshot};
use crate::git_journey::GitJourney;

const GIT: &str = "/usr/bin/git";

#[tokio::test]
#[cfg_attr(not(arany_ci), ignore = "native Linux account isolation")]
async fn typed_git_reads_run_natively_under_the_cage() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(workspace.join("cfg")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    host_git(GIT, &workspace, &home, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
    std::fs::write(workspace.join(".gitignore"), "cfg/private.json\n").unwrap();
    std::fs::write(
        workspace.join("cfg/private.json"),
        "GIT_DISCLOSURE_CANARY=initial\n",
    )
    .unwrap();
    host_git(GIT, &workspace, &home, &["add", "-A"]);
    host_git(GIT, &workspace, &home, &["add", "-f", "cfg/private.json"]);
    host_git(
        GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "first commit"],
    );
    std::fs::write(
        workspace.join("cfg/private.json"),
        "GIT_DISCLOSURE_CANARY=committed\n",
    )
    .unwrap();
    host_git(GIT, &workspace, &home, &["add", "-f", "cfg/private.json"]);
    host_git(
        GIT,
        &workspace,
        &home,
        &["commit", "-q", "-m", "GIT_DISCLOSURE_CANARY history"],
    );
    std::fs::write(workspace.join("src/a.txt"), "bravo\n").unwrap();
    host_git(
        GIT,
        &workspace,
        &home,
        &["commit", "-q", "-am", "second commit"],
    );
    std::fs::write(workspace.join(".env"), "private fixture\n").unwrap();
    host_git(GIT, &workspace, &home, &["add", ".env"]);
    host_git(
        GIT,
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
        GIT,
        &workspace,
        &home,
        &["update-ref", "refs/heads/protected", "HEAD"],
    );
    host_git(
        GIT,
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
    // The user's own configuration applies natively; its fsmonitor stays off under the cage.
    // Arany runs Git with the process HOME, which the isolated fixture points at its own home.
    let user_home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    std::fs::write(user_home.join("ignore"), "src/new.txt\n").unwrap();
    std::fs::write(
        user_home.join(".gitconfig"),
        format!(
            "[core]\n\texcludesFile = {}\n",
            user_home.join("ignore").display()
        ),
    )
    .unwrap();
    let repository_write = workspace.join(".git/write-probe");
    let monitor = home.join("fsmonitor");
    std::fs::write(
        &monitor,
        b"#!/usr/bin/sh\nset -eu\nprintf forbidden > .git/write-probe\nprintf 'baseline\\0/\\0'\n",
    )
    .unwrap();
    std::fs::set_permissions(&monitor, std::fs::Permissions::from_mode(0o700)).unwrap();
    {
        use std::io::Write;
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap();
        writeln!(config, "[core]\n\tfsmonitor = {}", monitor.display()).unwrap();
    }
    let plain_git = |args: &[&str]| {
        use crate::process::BoundedOutput;
        let output = std::process::Command::new(GIT)
            .args(args)
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &user_home)
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .expect("plain Git attack baseline");
        assert!(
            output.status.success(),
            "plain Git {args:?}: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    plain_git(&["--no-optional-locks", "status", "--porcelain=v1"]);
    assert_eq!(std::fs::read(&repository_write).unwrap(), b"forbidden");
    std::fs::remove_file(&repository_write).unwrap();
    let before = tree_snapshot(&workspace);

    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(
        &state_path.join("permissions.json"),
        br#"{"permissions":{"deny":["Read(./denied/**)"]}}"#,
    );
    let config = json!({
        "workspace_paths": ["."],
        "write": false,
        "commands": [],
        "skills": [],
        "mcp": [],
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
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{:?}",
        outcome.run.tools
    );
    assert_eq!(outcome.run.tools.len(), 9);

    let output = |index: usize| -> Value {
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
    assert!(
        !entries.contains(&json!({"path": "src/new.txt", "state": "??"})),
        "native Git applies the user's configuration from HOME"
    );
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

    crate::assert_git_range_outputs(&outcome.run, &workspace, &home, GIT);
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
    let catalog: Value =
        serde_json::from_str(requests[0].input.tools.as_ref().unwrap().catalog()).unwrap();
    assert_eq!(
        catalog["git"]["operations"],
        json!(["git_status", "git_diff", "git_log", "git_branch"])
    );
    let after = tree_snapshot(&workspace);
    let changed: Vec<_> = before
        .iter()
        .chain(&after)
        .filter(|entry| !(before.contains(entry) && after.contains(entry)))
        .map(|entry| &entry.0)
        .collect();
    assert!(
        changed.is_empty(),
        "Workspace or repository changed: {changed:?}"
    );

    std::fs::rename(workspace.join(".git"), home.join("repository-after-run")).unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
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

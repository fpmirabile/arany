use super::*;
use arany::permissions::PermissionSource;
use arany::test_support::git::{host_git, tree_snapshot};
use arany::{ApprovalMode, ToolCall, ToolDisposition};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const GIT: &str = "/usr/bin/git";

struct Repo {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    state: PathBuf,
    markers: PathBuf,
}

impl Repo {
    // A trusted-folder style grant over a repository whose configuration tries to run code.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        let markers = temp.path().join("markers");
        for directory in [workspace.join("src"), home.clone(), markers.clone()] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let git = |args: &[&str]| host_git(GIT, &workspace, &home, args);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(workspace.join("src/a.txt"), "bravo\n").unwrap();
        git(&["commit", "-q", "-am", "second"]);
        std::fs::write(workspace.join("src/a.txt"), "delta\n").unwrap();
        let hooks = temp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for name in ["pre-commit", "post-commit", "post-checkout", "fsmonitor"] {
            let path = hooks.join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\n/usr/bin/touch {}/{name}\n", markers.display()),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        {
            use std::io::Write;
            let mut config = std::fs::OpenOptions::new()
                .append(true)
                .open(workspace.join(".git/config"))
                .unwrap();
            write!(
                config,
                "[core]\n\thooksPath = {}\n\tfsmonitor = {}\n\tsshCommand = {}\n",
                hooks.display(),
                hooks.join("fsmonitor").display(),
                hooks.join("fsmonitor").display(),
            )
            .unwrap();
        }
        let state = temp.path().join("state");
        StateRoot::admit(&state).unwrap();
        let executable = std::fs::canonicalize(GIT).unwrap();
        let config = json!({
            "version": 1,
            "workspace_paths": ["."],
            "write": true,
            "commands": [],
            "skills": [],
            "mcp": [],
            "git": {
                "name": "git",
                "executable": executable,
                "sha256": Sha256::digest(std::fs::read(&executable).unwrap())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                "interpreter": false,
                "inputs": []
            },
            "git_identity": {"name": "Arany Journey", "email": "journey@example.invalid"}
        });
        let tools = state.join("tools.json");
        std::fs::write(&tools, serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Present settings journal a verdict for every step.
        let permissions = state.join("permissions.json");
        std::fs::write(&permissions, br#"{"permissions":{}}"#).unwrap();
        std::fs::set_permissions(&permissions, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            _temp: temp,
            workspace,
            home,
            state,
            markers,
        }
    }

    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new(GIT)
            // The observer must not run the hostile hooks or monitor itself.
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    // Runs one primary Run, answering each human approval; `lose` drops that approval's Guard receipt.
    async fn run(
        &self,
        outcomes: Vec<ProviderOutcome>,
        lose: bool,
    ) -> (Result<arany::RunOutcome, EngineError>, usize, usize, Engine) {
        let engine = engine_support::open(
            StateRoot::open_existing(&self.state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let (approvals, mut inbox) = arany::ToolApprovals::new(ApprovalMode::AutoEdits);
        let mut input = request(&self.workspace, "Commit, branch and switch");
        input.include_paths.clear();
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        };
        let mut asked = 0;
        let outcome = {
            let running = run_with(&engine, &provider, input, controls);
            tokio::pin!(running);
            loop {
                tokio::select! {
                    biased;
                    outcome = &mut running => break outcome,
                    Some(approval) = inbox.next() => {
                        asked += 1;
                        if lose {
                            arany::FaultPlan::lose_guard_receipt(approval.intent.id);
                        }
                        approval.decide(true);
                    }
                }
            }
        };
        let calls = provider.requests.lock().unwrap().len();
        (outcome, asked, calls, engine)
    }

    async fn reopen(
        &self,
        engine: Engine,
        session: SessionId,
    ) -> (Vec<arany::EventEnvelope>, SessionView) {
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&self.state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = SessionView::replay(session, &events).unwrap().unwrap();
        store.close().await.unwrap();
        (events, view)
    }
}

fn output(tool: &arany::ToolView) -> Value {
    serde_json::from_str(&tool.observation.as_ref().unwrap().output).unwrap()
}

#[tokio::test]
async fn git_writes_commit_branch_and_switch_through_the_guard_and_replay_without_effects() {
    let repo = Repo::new();
    let first = repo.git(&["rev-parse", "HEAD~1"]).trim().to_owned();
    let calls = [
        ToolCall::GitCommit {
            message: "third".into(),
            paths: vec!["src/a.txt".into()],
            staged: false,
        },
        ToolCall::GitBranch {
            name: Some("older".into()),
            start_point: Some("HEAD~2".into()),
        },
        ToolCall::GitBranch {
            name: None,
            start_point: None,
        },
        ToolCall::GitSwitch {
            branch: "older".into(),
        },
    ];
    let mut outcomes: Vec<_> = calls.iter().cloned().map(tool_outcome).collect();
    outcomes.push(ProviderOutcome::Answer(Answer {
        text: "Committed and switched".into(),
    }));
    let (outcome, asked, provider_calls, engine) = repo.run(outcomes, false).await;
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{:?}",
        outcome.run.tools
    );
    assert_eq!(
        asked, 3,
        "each Git write asks a human even in Auto edits; the list does not"
    );
    assert_eq!(provider_calls, 5);

    assert_eq!(
        repo.git(&["log", "-1", "--format=%s|%an <%ae>", "main"]),
        "third|Arany Journey <journey@example.invalid>\n"
    );
    assert_eq!(repo.git(&["symbolic-ref", "HEAD"]), "refs/heads/older\n");
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), first);
    assert_eq!(
        std::fs::read_to_string(repo.workspace.join("src/a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(repo.git(&["status", "--porcelain=v1"]), "");
    assert_eq!(
        std::fs::read_dir(&repo.markers).unwrap().count(),
        0,
        "repository hooks and monitors never ran"
    );

    let after = tree_snapshot(&repo.workspace);
    let (events, view) = repo.reopen(engine, outcome.session_id).await;
    assert_eq!(
        tree_snapshot(&repo.workspace),
        after,
        "reopening never re-executes"
    );
    let tools = &view.runs[0].tools;
    assert_eq!(tools.len(), 4);
    for (tool, call) in tools.iter().zip(&calls) {
        assert_eq!(&tool.intent.call, call);
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{}",
            observation.output
        );
        assert!(matches!(
            observation.attestation,
            Some(arany::Attestation::Guard(_))
        ));
    }
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool
                .permission_verdict
                .as_ref()
                .map(|verdict| verdict.source))
            .collect::<Vec<_>>(),
        [
            Some(PermissionSource::Human),
            Some(PermissionSource::Human),
            Some(PermissionSource::Mode),
            Some(PermissionSource::Human)
        ]
    );
    let committed = output(&tools[0]);
    assert_eq!(committed["branch"], "main");
    assert_eq!(
        committed["publication"]["ref"]["after"],
        repo.git(&["rev-parse", "main"]).trim()
    );
    assert_eq!(output(&tools[1])["oid"], first);
    assert_eq!(output(&tools[2])["branches"].as_array().unwrap().len(), 2);
    assert_eq!(
        output(&tools[3])["publication"]["head"]["after"],
        "ref: refs/heads/older"
    );
    let started = events
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
        .count();
    assert_eq!(started, 4);
}

#[tokio::test]
async fn a_lost_git_publication_receipt_stays_uncertain_and_is_never_retried() {
    let repo = Repo::new();
    let before = repo.git(&["rev-parse", "HEAD"]);
    let commit = ToolCall::GitCommit {
        message: "third".into(),
        paths: vec!["src/a.txt".into()],
        staged: false,
    };
    let (outcome, asked, provider_calls, engine) =
        repo.run(vec![tool_outcome(commit.clone())], true).await;
    assert!(
        matches!(
            outcome,
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
        ),
        "{:?}",
        outcome.map(|outcome| outcome.run.status)
    );
    assert_eq!((asked, provider_calls), (1, 1), "no Provider call follows");
    // The Guard did publish; only its receipt was lost, so the journal must not claim either way.
    assert_ne!(repo.git(&["rev-parse", "HEAD"]), before);
    let session = list_sessions(
        StateRoot::open_existing(&repo.state).unwrap(),
        repo.workspace.clone(),
    )
    .await
    .unwrap()[0]
        .id;
    let (events, view) = repo.reopen(engine, session).await;
    let started = events
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
        .count();
    assert_eq!(started, 1, "never retried");
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    let tool = &view.runs[0].tools[0];
    assert_eq!(tool.intent.call, commit);
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        ToolDisposition::Uncertain
    );
}

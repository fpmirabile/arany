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
        self.run_replying(outcomes, lose, |_| arany::ApprovalReply::AllowOnce)
            .await
    }

    async fn run_replying(
        &self,
        outcomes: Vec<ProviderOutcome>,
        lose: bool,
        mut reply: impl FnMut(&arany::ToolApproval) -> arany::ApprovalReply,
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
                        let answer = reply(&approval);
                        approval.decide(answer);
                    }
                }
            }
        };
        let calls = provider.requests.lock().unwrap().len();
        (outcome, asked, calls, engine)
    }

    fn settings(&self, document: Value) {
        let permissions = self.state.join("permissions.json");
        std::fs::write(&permissions, serde_json::to_vec(&document).unwrap()).unwrap();
        std::fs::set_permissions(&permissions, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn saved_rules(&self) -> Vec<arany::permissions::TypedPrefixEntry> {
        arany::permissions::PermissionSettings::load(
            &StateRoot::open_existing(&self.state).unwrap(),
            &self.workspace,
        )
        .unwrap()
        .typed_prefixes()
    }

    fn edit(&self, edit: arany::permissions::TypedPrefixEdit) {
        let state = StateRoot::open_existing(&self.state).unwrap();
        arany::permissions::PermissionSettings::load(&state, &self.workspace)
            .unwrap()
            .save_edit(&state, &self.workspace, edit)
            .unwrap();
    }

    fn change(&self, text: &str) {
        std::fs::write(self.workspace.join("src/a.txt"), text).unwrap();
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

fn commit(message: &str) -> ToolCall {
    ToolCall::GitCommit {
        message: message.into(),
        paths: vec!["src/a.txt".into()],
        staged: false,
    }
}

fn answer() -> ProviderOutcome {
    ProviderOutcome::Answer(Answer {
        text: "Done".into(),
    })
}

fn verdict_of(view: &SessionView, run: usize) -> arany::permissions::PermissionVerdict {
    view.runs[run].tools[0]
        .permission_verdict
        .clone()
        .expect("verdict")
}

#[tokio::test]
async fn goal_continuations_readmit_saved_typed_rules_but_latch_external_drift() {
    for drift in [false, true] {
        let repo = Repo::new();
        let path = repo.state.join("tools.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let find = std::fs::canonicalize("/usr/bin/find").unwrap();
        config["commands"] = json!([{
            "name":"find", "executable":find,
            "sha256":Sha256::digest(std::fs::read(&find).unwrap()).iter()
                .map(|byte| format!("{byte:02x}")).collect::<String>(),
            "interpreter":false, "inputs":[]
        }]);
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        let engine = engine_support::open(
            StateRoot::open_existing(&repo.state).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut input = request(&repo.workspace, "Inspect after remembering a commit rule");
        input.include_paths.clear();
        let budget = arany::GoalBudget {
            max_runs: 3,
            ..Default::default()
        };
        input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
        let plan = arany::GoalPlanContract {
            mission: input.objective.clone(),
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec![".".into()],
            commands: vec!["find".into()],
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/a.txt".into(),
                content: "unreached criterion".into(),
            },
            budget,
        };
        let settings = repo.state.join("permissions.json");
        let provider = Arc::new(
            ScriptedProvider::new(vec![
                plan_outcome(plan),
                answer(),
                tool_outcome(commit("third")),
                answer(),
                tool_outcome(ToolCall::CommandUnguarded {
                    program: "find".into(),
                    args: vec![".".into()],
                    cwd: String::new(),
                    justification: "Inspect after saving the approved typed rule".into(),
                }),
                answer(),
            ])
            .with_before_call(3, move || {
                if drift {
                    let mut bytes = std::fs::read(&settings).unwrap();
                    bytes.push(b' ');
                    std::fs::write(&settings, bytes).unwrap();
                }
            }),
        );
        let (approvals, mut inbox) = arany::ToolApprovals::new(ApprovalMode::Request);
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        };
        let mut prompted = 0;
        let outcome = {
            let running = run_with(&engine, &provider, input, controls);
            tokio::pin!(running);
            loop {
                tokio::select! {
                    result = &mut running => break result.unwrap(),
                    Some(approval) = inbox.next() => {
                        prompted += 1;
                        if approval.unguarded.is_some() {
                            approval.decide(arany::ApprovalReply::AllowOnce);
                        } else {
                            assert_eq!(approval.remember_prefix, Some(arany::permissions::TypedGitPrefix::Commit));
                            approval.decide(arany::ApprovalReply::AllowPrefixInFolder);
                        }
                    }
                }
            }
        };
        assert_eq!(provider.requests.lock().unwrap().len(), 6);
        assert_eq!(prompted, if drift { 1 } else { 2 });
        let (_, view) = repo.reopen(engine, outcome.session_id).await;
        assert_eq!(view.runs.len(), 3);
        let observation = view.runs[2].tools[0].observation.as_ref().unwrap();
        if drift {
            assert_eq!(observation.disposition, ToolDisposition::Denied);
            assert!(observation.attestation.is_none());
            assert!(
                observation
                    .output
                    .contains("permission settings changed outside admission")
            );
        } else {
            assert_eq!(observation.disposition, ToolDisposition::Succeeded);
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::Unguarded(_))
            ));
        }
        let digest = |index: usize| {
            view.runs[index]
                .config
                .as_ref()
                .unwrap()
                .tool_policy
                .as_ref()
                .unwrap()
                .permissions_digest
        };
        assert_eq!(digest(0), digest(1));
        assert_eq!(digest(1) == digest(2), drift);
        assert_eq!(repo.saved_rules().len(), 1);
    }
}

#[tokio::test]
async fn remembered_git_prefix_applies_next_run_and_revocation_survives_restart() {
    use arany::permissions::{
        Decision, PermissionMode, PermissionSource, TypedGitPrefix, TypedPrefixEdit,
    };
    let repo = Repo::new();
    let mut offered = Vec::new();
    let (outcome, asked, _, engine) = repo
        .run_replying(
            vec![
                tool_outcome(commit("third")),
                tool_outcome(ToolCall::CommandUnguarded {
                    program: "find".into(),
                    args: vec![".".into()],
                    cwd: String::new(),
                    justification: "Inspect after saving the approved typed rule".into(),
                }),
                tool_outcome(commit("again")),
                answer(),
            ],
            false,
            |approval| {
                offered.push(approval.remember_prefix);
                if offered.len() == 1 {
                    arany::ApprovalReply::AllowPrefixInFolder
                } else {
                    arany::ApprovalReply::AllowOnce
                }
            },
        )
        .await;
    let first = outcome.unwrap();
    assert_eq!(asked, 2, "the saved rule applies only from the next Run");
    let refused = first.run.tools[1].observation.as_ref().unwrap();
    assert_eq!(refused.disposition, ToolDisposition::Denied);
    assert!(
        refused
            .output
            .contains("permission settings changed outside admission")
    );
    assert!(refused.attestation.is_none());
    assert_eq!(
        offered,
        [Some(TypedGitPrefix::Commit), Some(TypedGitPrefix::Commit)]
    );
    for drift in [false, true] {
        if drift {
            let path = repo.state.join("permissions.json");
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.push(b' ');
            std::fs::write(path, bytes).unwrap();
        }
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_outcome(ToolCall::CommandUnguarded {
                program: "find".into(),
                args: vec![
                    repo.workspace.to_string_lossy().into_owned(),
                    "-maxdepth".into(),
                    "0".into(),
                ],
                cwd: String::new(),
                justification: "Inspect the repository after saving the approved typed rule".into(),
            }),
            answer(),
        ]));
        let (approvals, mut inbox) = arany::ToolApprovals::new(ApprovalMode::Request);
        let mut input = request(&repo.workspace, "Inspect the repository");
        input.session_id = Some(first.session_id);
        input.title = None;
        input.include_paths.clear();
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        };
        let mut prompted = 0;
        let next = {
            let running = run_with(&engine, &provider, input, controls);
            tokio::pin!(running);
            loop {
                tokio::select! {
                    result = &mut running => break result.unwrap(),
                    Some(approval) = inbox.next() => {
                        assert!(approval.unguarded.is_some());
                        prompted += 1;
                        approval.decide(arany::ApprovalReply::Deny);
                    }
                }
            }
        };
        assert_eq!(prompted, usize::from(!drift));
        let observation = next.run.tools.last().unwrap().observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Denied);
        assert!(observation.output.contains(if drift {
            "permission settings changed outside admission"
        } else {
            "Approval was not granted"
        }));
        assert_eq!(next.run.unguarded_tools(), 0);
    }
    let (_, view) = repo.reopen(engine, first.session_id).await;
    assert_eq!(view.runs[0].tools[1].observation.as_ref(), Some(refused));
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (verdict.source, verdict.basis, verdict.approved),
        (PermissionSource::Human, PermissionSource::Mode, true)
    );
    let rules = repo.saved_rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (
            rules[0].prefix,
            rules[0].decision,
            rules[0].folder.as_deref()
        ),
        (
            TypedGitPrefix::Commit,
            Decision::Allow,
            repo.workspace.to_str()
        )
    );
    assert_eq!(repo.git(&["log", "-1", "--format=%s", "main"]), "third\n");

    repo.change("echo\n");
    let (outcome, asked, _, engine) = repo
        .run_replying(
            vec![tool_outcome(commit("fourth")), answer()],
            false,
            |_| panic!("a remembered allow must not ask"),
        )
        .await;
    let second = outcome.unwrap();
    assert_eq!(asked, 0);
    assert_eq!(repo.git(&["log", "-1", "--format=%s", "main"]), "fourth\n");
    let after = tree_snapshot(&repo.workspace);
    let (events, view) = repo.reopen(engine, second.session_id).await;
    assert_eq!(tree_snapshot(&repo.workspace), after);
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (
            verdict.source,
            verdict.basis,
            verdict.mode,
            verdict.approved
        ),
        (
            PermissionSource::TypedPrefixRule,
            PermissionSource::TypedPrefixRule,
            Some(PermissionMode::Default),
            true
        )
    );
    assert!(matches!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .attestation,
        Some(arany::Attestation::Guard(_))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
            .count(),
        1
    );

    repo.edit(TypedPrefixEdit::Replace {
        index: 0,
        decision: Decision::Deny,
    });
    repo.change("foxtrot\n");
    let head = repo.git(&["rev-parse", "HEAD"]);
    let (outcome, asked, _, engine) = repo
        .run_replying(vec![tool_outcome(commit("fifth")), answer()], false, |_| {
            panic!("a deny rule must not ask")
        })
        .await;
    let denied = outcome.unwrap();
    assert_eq!(asked, 0);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    let (_, view) = repo.reopen(engine, denied.session_id).await;
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (verdict.decision, verdict.source, verdict.approved),
        (Decision::Deny, PermissionSource::TypedPrefixRule, false)
    );
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        ToolDisposition::Denied
    );

    repo.edit(TypedPrefixEdit::Remove { index: 0 });
    assert!(repo.saved_rules().is_empty());
    let (outcome, asked, _, engine) = repo
        .run_replying(
            vec![tool_outcome(commit("sixth")), answer()],
            false,
            |approval| {
                assert_eq!(approval.remember_prefix, Some(TypedGitPrefix::Commit));
                arany::ApprovalReply::AllowOnce
            },
        )
        .await;
    let revoked = outcome.unwrap();
    assert_eq!(asked, 1, "revocation restores the prompt");
    let (_, view) = repo.reopen(engine, revoked.session_id).await;
    assert_eq!(verdict_of(&view, 0).source, PermissionSource::Human);
    assert_eq!(repo.git(&["log", "-1", "--format=%s", "main"]), "sixth\n");
}

#[tokio::test]
async fn remembering_is_not_offered_without_settings_and_never_activates_them() {
    use arany::permissions::PermissionSource;
    let repo = Repo::new();
    std::fs::remove_file(repo.state.join("permissions.json")).unwrap();
    let (outcome, asked, _, engine) = repo
        .run_replying(
            vec![tool_outcome(commit("third")), answer()],
            false,
            |approval| {
                assert_eq!(approval.remember_prefix, None);
                arany::ApprovalReply::AllowOnce
            },
        )
        .await;
    let first = outcome.unwrap();
    assert_eq!(asked, 1);
    let (_, view) = repo.reopen(engine, first.session_id).await;
    assert!(
        view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .tool_policy
            .as_ref()
            .unwrap()
            .permissions_digest
            .is_none()
    );
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (verdict.mode, verdict.source, verdict.approved),
        (None, PermissionSource::Human, true)
    );
    assert!(!repo.state.join("permissions.json").exists());
}

#[tokio::test]
async fn a_failed_rule_save_after_the_verdict_dispatches_nothing_and_replays() {
    use arany::permissions::PermissionSource;
    let repo = Repo::new();
    let elsewhere = repo._temp.path().join("elsewhere");
    std::fs::write(&elsewhere, b"keep").unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.state.join("permissions.pending")).unwrap();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let (outcome, asked, _, engine) = repo
        .run_replying(vec![tool_outcome(commit("third")), answer()], false, |_| {
            arany::ApprovalReply::AllowPrefixInFolder
        })
        .await;
    let outcome = outcome.unwrap();
    assert_eq!(asked, 1);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head, "nothing dispatched");
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
    assert!(repo.saved_rules().is_empty());
    let (_, view) = repo.reopen(engine, outcome.session_id).await;
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (verdict.source, verdict.approved),
        (PermissionSource::Human, true),
        "the human decision is journaled before the save"
    );
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        ToolDisposition::Denied
    );
}

#[tokio::test]
async fn an_unconfirmed_rule_save_is_uncertain_stops_the_run_and_never_dispatches_or_resaves() {
    use arany::permissions::PermissionSource;
    let repo = Repo::new();
    let head = repo.git(&["rev-parse", "HEAD"]);
    arany::FaultPlan::fail_record_sync(repo.state.clone(), "permissions.json");
    let (outcome, asked, provider_calls, engine) = repo
        .run_replying(vec![tool_outcome(commit("third")), answer()], false, |_| {
            arany::ApprovalReply::AllowPrefixInFolder
        })
        .await;
    let status = outcome.map(|outcome| outcome.run.status);
    assert_eq!(
        (asked, provider_calls),
        (1, 1),
        "no continuation: {status:?}"
    );
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head, "nothing dispatched");
    let session = list_sessions(
        StateRoot::open_existing(&repo.state).unwrap(),
        repo.workspace.clone(),
    )
    .await
    .unwrap()[0]
        .id;
    let saved = std::fs::read(repo.state.join("permissions.json")).unwrap();
    let (events, view) = repo.reopen(engine, session).await;
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    let verdict = verdict_of(&view, 0);
    assert_eq!(
        (verdict.source, verdict.approved),
        (PermissionSource::Human, true)
    );
    let observation = view.runs[0].tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Uncertain);
    assert!(observation.output.contains("rule save not confirmed"));
    assert_eq!(
        events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read(repo.state.join("permissions.json")).unwrap(),
        saved,
        "replay never saves again"
    );
}

#[tokio::test]
async fn typed_prefix_rules_cannot_expand_git_authority() {
    let repo = Repo::new();
    let allow = |workspace: Value| {
        json!({"permissions": {"typedPrefixes": [
            {"prefix": ["git", "commit"], "decision": "allow", "workspace": workspace}
        ]}})
    };
    // Repository content never adds rules.
    for name in ["permissions.json", ".arany-permissions.json"] {
        std::fs::write(
            repo.workspace.join(name),
            serde_json::to_vec(&allow(Value::Null)).unwrap(),
        )
        .unwrap();
    }
    let metadata = std::fs::metadata(&repo.workspace).unwrap();
    let wrong = |inode: u64| {
        allow(json!({
            "path": repo.workspace,
            "device": std::os::unix::fs::MetadataExt::dev(&metadata),
            "inode": inode,
        }))
    };
    for (document, label) in [
        (json!({"permissions": {}}), "repository files only"),
        (
            wrong(std::os::unix::fs::MetadataExt::ino(&metadata) + 1),
            "another folder identity",
        ),
    ] {
        repo.settings(document);
        let (outcome, asked, _, engine) = repo
            .run_replying(vec![tool_outcome(commit("third")), answer()], false, |_| {
                arany::ApprovalReply::Deny
            })
            .await;
        let outcome = outcome.unwrap();
        assert_eq!(asked, 1, "{label}");
        let (_, view) = repo.reopen(engine, outcome.session_id).await;
        assert!(!verdict_of(&view, 0).approved, "{label}");
    }
    assert_eq!(repo.git(&["log", "-1", "--format=%s", "main"]), "second\n");
}

#[test]
fn host_git_pr_journey_commits_human_verdict_replays_unknown_and_refuses_missing_human() {
    use arany::test_support::process::BoundedOutput;
    const TEST: &str = "git_writes::host_git_pr_journey_commits_human_verdict_replays_unknown_and_refuses_missing_human";
    const CHILD: &str = "ARANY_HOST_GIT_JOURNEY";
    const CANARY: &str = "synthetic-host-credential-canary";
    if std::env::var_os(CHILD).is_none() {
        let auth = tempfile::tempdir().unwrap();
        let home = auth.path().join("home");
        let gh = home.join(".config/gh");
        std::fs::create_dir_all(&gh).unwrap();
        let hosts = gh.join("hosts.yml");
        std::fs::write(&hosts, format!("github.com:\n    oauth_token: {CANARY}\n")).unwrap();
        std::fs::set_permissions(hosts, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env(CHILD, "1")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GH_CONFIG_DIR", gh)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "TMPDIR",
                std::fs::canonicalize(std::env::temp_dir()).unwrap(),
            )
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .bounded_output_for(Duration::from_secs(60), 64 * 1024)
            .unwrap();
        assert!(
            result.status.success(),
            "isolated host journey: stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        process::assert_single_test_completed(&result.stdout, TEST);
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for scenario in ["success", "missing_human", "uncertain"] {
            let human = scenario != "missing_human";
            let success = scenario == "success";
            let repo = Repo::new();
            let tools_path = repo.state.join("tools.json");
            let mut tools: Value = serde_json::from_slice(&std::fs::read(&tools_path).unwrap()).unwrap();
            // This synthetic peer succeeds if accidentally dispatched, so missing-human refusal cannot pass on a peer error.
            let executable = std::fs::canonicalize(if scenario == "uncertain" { "/usr/bin/false" } else { "/usr/bin/true" }).unwrap();
            tools["commands"] = json!([{"name":"gh", "executable":executable, "sha256":Sha256::digest(std::fs::read(&executable).unwrap()).iter().map(|b| format!("{b:02x}")).collect::<String>(), "interpreter":false, "inputs":[]}]);
            std::fs::write(tools_path, serde_json::to_vec(&tools).unwrap()).unwrap();
            repo.settings(json!({"permissions":{"defaultMode":"auto", "typedPrefixes":[{"prefix":["gh","pr","create"],"decision":"allow","workspace":null}]}}));
            let call = ToolCall::GhPrCreate { repository: "github.com/example/project".into(), base: "base".into(), head: "main".into(), title: "Synthetic PR".into(), body: "Synthetic body".into(), draft: true };
            let mut outcomes = vec![tool_outcome(call)];
            if success { outcomes.push(ProviderOutcome::Answer(Answer { text: "PR created".into() })); }
            let provider = Arc::new(ScriptedProvider::new(outcomes));
            let engine = engine_support::open(StateRoot::open_existing(&repo.state).unwrap(), EngineConfig { guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()), ..Default::default() }).unwrap();
            let (approvals, mut inbox) = arany::ToolApprovals::new(ApprovalMode::Auto);
            let mut input = request(&repo.workspace, "Create the explicitly approved PR");
            input.include_paths.clear();
            let controls = RunControls { tools: ToolGrant::Configured, approvals: human.then_some(approvals), ..Default::default() };
            let mut asked = 0;
            let outcome = {
                let running = run_with(&engine, &provider, input, controls);
                tokio::pin!(running);
                loop {
                    tokio::select! {
                        result = &mut running => break result,
                        Some(approval) = inbox.next() => {
                            asked += 1;
                            assert!(approval.intent.call.is_host() && approval.intent.host.is_some());
                            assert!(approval.remember_prefix.is_none() && approval.unguarded.is_none());
                            let contender = engine_support::open(StateRoot::open_existing(&repo.state).unwrap(), EngineConfig { guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()), ..Default::default() }).unwrap();
                            let peer = Arc::new(ScriptedProvider::new(vec![answer()]));
                            let mut competing = request(&repo.workspace, "A competing host write");
                            competing.include_paths.clear();
                            let refused = run_with(&contender, &peer, competing, RunControls { tools: ToolGrant::Configured, ..Default::default() }).await;
                            assert!(matches!(refused, Err(arany::EngineError::WorkspaceBusy)));
                            assert!(peer.requests.lock().unwrap().is_empty());
                            contender.close().await.unwrap();
                            approval.decide(arany::ApprovalReply::AllowOnce);
                        }
                    }
                }
            };
            assert_eq!(asked, usize::from(human));
            assert_eq!(provider.requests.lock().unwrap().len(), if success { 2 } else { 1 });
            assert!(!format!("{:?}", provider.requests.lock().unwrap()).contains(CANARY));
            let session = match outcome {
                Ok(outcome) => outcome.session_id,
                Err(arany::EngineError::Tool(arany::ToolError::Uncertain)) if scenario == "uncertain" => {
                    list_sessions(StateRoot::open_existing(&repo.state).unwrap(), repo.workspace.clone()).await.unwrap()[0].id
                }
                Err(error) => panic!("{scenario}: {error:?}"),
            };
            let (events, view) = repo.reopen(engine, session).await;
            for event in &events { assert!(!event.event.payload().unwrap().contains(CANARY)); }
            let tool = view.runs[0].tools.iter().find(|tool| tool.intent.call.is_host()).unwrap();
            let verdict = tool.permission_verdict.as_ref().unwrap();
            let observation = tool.observation.as_ref().unwrap();
            assert_eq!(verdict.approved, human);
            assert_eq!(observation.disposition, if success { ToolDisposition::Succeeded } else if human { ToolDisposition::Uncertain } else { ToolDisposition::Denied });
            assert_eq!(matches!(observation.attestation, Some(arany::Attestation::Host(_))), human);
            if human { assert_eq!(verdict.source, PermissionSource::Human); }
            assert_eq!(std::fs::read_dir(&repo.markers).unwrap().count(), 0);
            let shown = observe_process(std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear().args(["show", "--state-dir"]).arg(&repo.state).args(["--output", "text"]).arg(session.to_string()), repo._temp.path(), 256 * 1024);
            assert!(shown.status.success());
            assert!(shown.stderr.is_empty());
            let text = String::from_utf8(shown.stdout).unwrap();
            assert!(text.contains("host") && !text.contains(CANARY));
        }
    });
}

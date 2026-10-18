use super::*;
use arany::permissions::{Decision, PermissionSource};
use arany::test_support::goals::assert_action_bounds;
use arany::{ApprovalMode, ApprovalReply, ToolCall, UnguardedPrompt};
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

const SCRIPT: &str = "if [ -n \"$ARANY_UNGUARDED\" ]; then echo ran > ran.txt; exit 0; fi; exit 7";

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn guarded(script: &str) -> ToolCall {
    ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: String::new(),
    }
}

fn unguarded(script: &str) -> ToolCall {
    ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: String::new(),
        justification: "The sandbox may have blocked it".into(),
    }
}

fn plan(call: &ToolCall) -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mission: "Complete the bounded Tool action".into(),
        paths: vec!["notes".into()],
        commands: vec!["sh".into()],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: vec![],
        mcp_tools: vec![],
        success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call.clone()),
        },
        budget: budget(),
    }
}

fn budget() -> arany::GoalBudget {
    arany::GoalBudget {
        max_provider_calls: Some(16),
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    }
}

struct Host {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    state: PathBuf,
    engine: Engine,
}

struct Seen {
    outcome: arany::RunOutcome,
    prompts: Vec<UnguardedPrompt>,
    requests: usize,
    unconsumed: usize,
}

impl Host {
    fn new(settings: Option<&Value>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let program = json!({"name":"sh","executable":shell,"sha256":hash(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]});
        private_file(
            &state.join("tools.json"),
            &serde_json::to_vec(&json!({
                "version":1,"workspace_paths":["notes"],"write":false,"commands":[program],"skills":[],"mcp":[]
            }))
            .unwrap(),
        );
        if let Some(settings) = settings {
            private_file(
                &state.join("permissions.json"),
                &serde_json::to_vec(settings).unwrap(),
            );
        }
        let engine = crate::engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        Self {
            _temp: temp,
            workspace,
            state,
            engine,
        }
    }

    fn rules(&self) -> Option<Value> {
        std::fs::read(self.state.join("unguarded-commands.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    async fn run(
        &self,
        workspace: &Path,
        outcomes: Vec<ProviderOutcome>,
        terminal: Option<ApprovalMode>,
        mut replies: Vec<ApprovalReply>,
    ) -> Seen {
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let (approvals, mut inbox) = arany::ToolApprovals::new(terminal.unwrap_or_default());
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: terminal.map(|_| approvals),
            ..Default::default()
        };
        let mut input = request(workspace, "Run the failing command");
        input.include_paths.clear();
        input.goal.budget = Some(budget());
        let running = run_with(&self.engine, &provider, input, controls);
        tokio::pin!(running);
        let mut prompts = Vec::new();
        replies.reverse();
        let outcome = loop {
            tokio::select! {
                biased;
                result = &mut running => break result.unwrap(),
                approval = inbox.next(), if terminal.is_some() => {
                    let approval = approval.unwrap();
                    match approval.unguarded.clone() {
                        Some(prompt) => {
                            prompts.push(prompt);
                            approval.decide(replies.pop().expect("a scripted reply"));
                        }
                        None => approval.decide(ApprovalReply::AllowOnce),
                    }
                }
            }
        };
        let requests = provider.requests.lock().unwrap().len();
        let unconsumed = provider.responses.lock().unwrap().len();
        let seen = Seen {
            outcome,
            prompts,
            requests,
            unconsumed,
        };
        for tool in &seen.outcome.run.tools {
            let observation = tool.observation.as_ref();
            assert!(
                observation.is_none_or(|o| o.disposition != arany::ToolDisposition::Uncertain),
                "no Tool ends Uncertain: {}",
                tool_report(observation)
            );
        }
        assert!(
            replies.is_empty(),
            "every scripted reply is consumed; first tool: {}",
            tool_report(
                seen.outcome
                    .run
                    .tools
                    .first()
                    .and_then(|tool| tool.observation.as_ref())
            )
        );
        seen
    }

    async fn reopen(self, session: SessionId) -> (Vec<arany::EventEnvelope>, SessionView) {
        self.engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&self.state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = SessionView::replay(session, &events).unwrap().unwrap();
        assert_action_bounds(&events, false, false);
        store.close().await.unwrap();
        (events, view)
    }
}

fn last_tool(seen: &Seen) -> &arany::ToolView {
    seen.outcome.run.tools.last().expect("a Tool ran")
}

fn script(
    first: &ToolCall,
    second: &ToolCall,
    extra: Vec<ProviderOutcome>,
) -> Vec<ProviderOutcome> {
    let mut outcomes = vec![
        tool_outcome(first.clone(), Some(Box::new(plan(second)))),
        tool_outcome(second.clone(), None),
    ];
    outcomes.extend(extra);
    outcomes.push(finish("answer"));
    outcomes
}

fn finish(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: "done".into(),
        result: text.into(),
    })
}

fn denied(seen: &Seen, needle: &str) {
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
    let tool = last_tool(seen);
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
    assert!(observation.attestation.is_none());
    assert!(
        observation.output.contains(needle),
        "{}",
        observation.output
    );
    let verdict = tool
        .permission_verdict
        .as_ref()
        .expect("a journaled verdict");
    assert!(!verdict.approved);
}

#[tokio::test]
async fn unguarded_rerun_is_offered_only_after_an_attested_guard_failure_and_runs_once_after_human_allow()
 {
    let host = Host::new(None);
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    let seen = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.unconsumed, 0);
    assert_eq!(seen.prompts.len(), 1);
    assert_eq!(
        seen.prompts[0].executable,
        std::fs::canonicalize("/bin/sh").unwrap().to_str().unwrap()
    );
    assert!(seen.prompts[0].attribution.is_none());
    assert_eq!(
        std::fs::read_to_string(host.workspace.join("ran.txt")).unwrap(),
        "ran\n"
    );
    let tool = last_tool(&seen);
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Unguarded(_))
    ));
    let verdict = tool.permission_verdict.as_ref().unwrap();
    assert!(verdict.approved && verdict.mode.is_none());
    assert_eq!(verdict.source, PermissionSource::Human);
    assert_eq!(seen.outcome.run.unguarded_tools(), 1);
    let (session, goal) = (seen.outcome.session_id, seen.outcome.goal_id);
    let (events, view) = host.reopen(session).await;
    let position = |matches: fn(&Event) -> bool| {
        events
            .iter()
            .position(|envelope| matches(&envelope.event))
            .expect("Event present")
    };
    let started = events
        .iter()
        .position(|envelope| {
            matches!(&envelope.event, Event::ToolStarted { intent, .. } if intent.call.is_unguarded())
        })
        .unwrap();
    let decided = position(
        |event| matches!(event, Event::ToolPermissionDecided { verdict, .. } if verdict.approved && verdict.source == PermissionSource::Human),
    );
    let finished = events
        .iter()
        .position(|envelope| {
            matches!(&envelope.event, Event::ToolFinished { observation, .. } if observation.intent.call.is_unguarded())
        })
        .unwrap();
    assert!(started < decided && decided < finished);
    assert_eq!(view.runs[0].unguarded_tools(), 1);
    let status = arany_cli::presentation::goal_status(&view);
    assert!(status.contains("unguarded effects: 1"), "{status}");
    let report = view.goal(goal).unwrap().report.as_ref().expect("report");
    assert!(
        report
            .evidence
            .iter()
            .any(|item| item.contains("An unguarded host run"))
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("unguarded host run(s) executed outside the Guard"))
    );
}

#[tokio::test]
async fn model_escalation_without_a_qualifying_failure_is_denied_and_journaled() {
    let cases: [(&str, ToolCall, ToolCall, &str); 4] = [
        (
            "no prior attempt",
            guarded("echo prior; exit 0"),
            unguarded(SCRIPT),
            "No identical command failed",
        ),
        (
            "one byte differs",
            guarded("exit 7"),
            unguarded("exit 8"),
            "No identical command failed",
        ),
        (
            "guard path alias",
            guarded("ls /workspace >/dev/null; exit 7"),
            unguarded("ls /workspace >/dev/null; exit 7"),
            "Guard path aliases",
        ),
        (
            "guard-only variable",
            guarded("echo \"$ARANY_TOOL_INPUTS\"; exit 7"),
            unguarded("echo \"$ARANY_TOOL_INPUTS\"; exit 7"),
            "Guard path aliases",
        ),
    ];
    for (case, first, second, needle) in cases {
        let host = Host::new(None);
        let seen = host
            .run(
                &host.workspace,
                script(&first, &second, vec![]),
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert!(seen.prompts.is_empty(), "{case}");
        assert_eq!(seen.unconsumed, 0, "{case}");
        denied(&seen, needle);
        assert!(!host.workspace.join("ran.txt").exists(), "{case}");
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(view.runs[0].unguarded_tools(), 0, "{case}");
    }
}

#[tokio::test]
async fn unguarded_rerun_is_never_ai_reviewed_and_dont_ask_or_deny_rules_refuse_it() {
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    let auto = Host::new(Some(&json!({"permissions":{"defaultMode":"auto"}})));
    let seen = auto
        .run(
            &auto.workspace,
            // The review of the guarded command is the only AI review in the script.
            script(&first, &second, vec![])
                .into_iter()
                .fold(Vec::new(), |mut all, outcome| {
                    let first_call = all.is_empty();
                    all.push(outcome);
                    if first_call {
                        all.push(finish("approve"));
                    }
                    all
                }),
            Some(ApprovalMode::Auto),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    assert_eq!(seen.requests, 4, "the unguarded run adds no review call");
    let verdict = last_tool(&seen).permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.source, PermissionSource::Human);
    assert_eq!(verdict.mode, Some(arany::permissions::PermissionMode::Auto));

    for (settings, source) in [
        (
            json!({"permissions":{"defaultMode":"dontAsk","allow":["Bash"]}}),
            PermissionSource::Mode,
        ),
        (
            json!({"permissions":{"allow":["Bash"],"deny":["command_unguarded"]}}),
            PermissionSource::Rule,
        ),
    ] {
        let host = Host::new(Some(&settings));
        let seen = host
            .run(
                &host.workspace,
                script(&first, &second, vec![]),
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert!(seen.prompts.is_empty(), "{settings}");
        let verdict = last_tool(&seen).permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, Decision::Deny, "{settings}");
        assert_eq!(verdict.source, source, "{settings}");
        assert!(!host.workspace.join("ran.txt").exists());
    }
}

fn write_rule(host: &Host, script: &str) {
    let metadata = std::fs::metadata(&host.workspace).unwrap();
    private_file(
        &host.state.join("unguarded-commands.json"),
        &serde_json::to_vec(&json!({
            "version": 1,
            "commands": [{
                "workspace": {
                    "path": host.workspace.to_str().unwrap(),
                    "device": metadata.dev(),
                    "inode": metadata.ino(),
                },
                "program": "sh",
                "args": ["-c", script],
                "cwd": "",
                "created_unix_ms": 1,
            }],
        }))
        .unwrap(),
    );
}

#[tokio::test]
async fn headless_exec_denies_unguarded_without_an_exact_rule_and_dispatches_with_one() {
    let host = Host::new(None);
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    let seen = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            None,
            vec![],
        )
        .await;
    denied(&seen, "needs interactive approval");
    assert_eq!(
        last_tool(&seen).permission_verdict.as_ref().unwrap().source,
        PermissionSource::UnavailableApproval
    );
    assert!(!host.workspace.join("ran.txt").exists());

    write_rule(&host, SCRIPT);
    let seen = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            None,
            vec![],
        )
        .await;
    let tool = last_tool(&seen);
    let verdict = tool.permission_verdict.as_ref().unwrap();
    assert!(verdict.approved);
    assert_eq!(verdict.source, PermissionSource::ExactCommandRule);
    assert!(host.workspace.join("ran.txt").is_file());
    assert_eq!(seen.outcome.run.unguarded_tools(), 1);
    let session = seen.outcome.session_id;
    let (_, view) = host.reopen(session).await;
    assert_eq!(view.runs[0].unguarded_tools(), 1);
}

#[tokio::test]
async fn an_ask_rule_still_denies_headless_unguarded_runs_with_an_exact_rule() {
    let host = Host::new(Some(
        &json!({"permissions":{"allow":["Bash"],"ask":["command_unguarded"]}}),
    ));
    write_rule(&host, SCRIPT);
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    let seen = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            None,
            vec![],
        )
        .await;
    denied(&seen, "needs interactive approval");
    assert!(!host.workspace.join("ran.txt").exists());
}

#[tokio::test]
async fn always_allow_records_the_exact_rule_for_this_project_only_and_applies_next_submission() {
    let host = Host::new(None);
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    // A repeat inside the same Run prompts again because the admitted snapshot is immutable.
    let seen = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(first.clone(), Some(Box::new(plan(&second)))),
                tool_outcome(second.clone(), None),
                tool_outcome(second.clone(), None),
                finish("answer"),
            ],
            Some(ApprovalMode::Request),
            vec![
                ApprovalReply::AllowAlwaysInProject,
                ApprovalReply::AllowOnce,
            ],
        )
        .await;
    assert_eq!(seen.prompts.len(), 2);
    let rules = host.rules().expect("a recorded rule");
    let commands = rules["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 1);
    let metadata = std::fs::metadata(&host.workspace).unwrap();
    assert_eq!(
        commands[0]["workspace"]["path"],
        host.workspace.to_str().unwrap()
    );
    assert_eq!(commands[0]["workspace"]["device"], metadata.dev());
    assert_eq!(commands[0]["workspace"]["inode"], metadata.ino());
    assert_eq!(commands[0]["args"], json!(["-c", SCRIPT]));

    let next = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(next.prompts.is_empty(), "the exact rule approves it");
    let verdict = last_tool(&next).permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.source, PermissionSource::ExactCommandRule);
    assert!(verdict.approved);

    let other = host._temp.path().join("other");
    std::fs::create_dir_all(other.join("notes")).unwrap();
    let other = other.canonicalize().unwrap();
    let elsewhere = host
        .run(
            &other,
            script(&first, &second, vec![]),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::Deny],
        )
        .await;
    assert_eq!(
        elsewhere.prompts.len(),
        1,
        "another Workspace prompts again"
    );
    assert!(!other.join("ran.txt").exists());
}

#[tokio::test]
async fn a_denied_prompt_dispatches_nothing() {
    let host = Host::new(None);
    let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
    let seen = host
        .run(
            &host.workspace,
            script(&first, &second, vec![]),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::Deny],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    denied(&seen, "Approval was not granted");
    assert!(!host.workspace.join("ran.txt").exists());
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
}

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
    restricted_approvals: bool,
    session: Option<SessionId>,
    policy: CollaborationPolicy,
}

struct Seen {
    outcome: arany::RunOutcome,
    prompts: Vec<UnguardedPrompt>,
    requests: usize,
    unconsumed: usize,
    calls: Vec<ProviderRequest>,
    intents: Vec<arany::EffectIntent>,
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
            restricted_approvals: false,
            session: None,
            policy: CollaborationPolicy::Single,
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
        let approvals = if self.restricted_approvals {
            approvals.without_human_only()
        } else {
            approvals
        };
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: terminal.map(|_| approvals),
            ..Default::default()
        };
        let mut input = request(workspace, "Run the failing command");
        input.session_id = self.session;
        input.include_paths.clear();
        input.policy = self.policy;
        input.goal.budget = Some(budget());
        let running = run_with(&self.engine, &provider, input, controls);
        tokio::pin!(running);
        let mut prompts = Vec::new();
        let mut intents = Vec::new();
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
                            intents.push(approval.intent.clone());
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
            calls: provider.requests.lock().unwrap().clone(),
            intents,
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
        arany::test_support::scenario::plan_outcome(plan(second)),
        tool_outcome(first.clone()),
        tool_outcome(second.clone()),
    ];
    outcomes.extend(extra);
    outcomes.push(finish("answer"));
    outcomes
}

fn finish(text: &str) -> ProviderOutcome {
    ProviderOutcome::Answer(Answer { text: text.into() })
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
async fn primary_host_request_needs_no_prior_failure_and_replays_after_human_allow() {
    let host = Host::new(None);
    let second = unguarded(SCRIPT);
    let seen = host
        .run(
            &host.workspace,
            vec![
                arany::test_support::scenario::plan_outcome(plan(&second)),
                tool_outcome(second.clone()),
                finish("answer"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.unconsumed, 0);
    assert_eq!(seen.prompts.len(), 1);
    assert!(seen.intents[0].host.is_some());
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
async fn guard_aliases_are_never_translated_into_host_paths() {
    let cases: [(&str, ToolCall, ToolCall, &str); 2] = [
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
            // The review of the guarded command, after the plan call, is the only AI review in the script.
            script(&first, &second, vec![])
                .into_iter()
                .fold(Vec::new(), |mut all, outcome| {
                    all.push(outcome);
                    if all.len() == 2 {
                        all.push(ProviderOutcome::Review(arany::ReviewVerdict::Approve));
                    }
                    all
                }),
            Some(ApprovalMode::Auto),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    assert_eq!(seen.requests, 5, "the unguarded run adds no review call");
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
async fn headless_host_commands_refuse_with_or_without_inactive_exact_rules() {
    for previous in [false, true] {
        let host = Host::new(None);
        if previous {
            write_rule(&host, SCRIPT);
        }
        let call = unguarded(SCRIPT);
        let seen = host
            .run(
                &host.workspace,
                vec![
                    arany::test_support::scenario::plan_outcome(plan(&call)),
                    tool_outcome(call),
                ],
                None,
                vec![],
            )
            .await;
        denied(&seen, "needs fresh human approval");
        assert!(seen.prompts.is_empty());
        assert!(!host.workspace.join("ran.txt").exists());
        assert_eq!(host.rules().is_some(), previous);
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(view.runs[0].unguarded_tools(), 0);
    }
}

#[tokio::test]
async fn restricted_approval_channels_never_dispatch_unguarded_even_with_an_exact_rule() {
    for exact_rule in [false, true] {
        let mut host = Host::new(None);
        host.restricted_approvals = true;
        if exact_rule {
            write_rule(&host, SCRIPT);
        }
        let (first, second) = (guarded(SCRIPT), unguarded(SCRIPT));
        let mut outcomes = script(&first, &second, vec![]);
        outcomes.pop();
        let seen = host
            .run(
                &host.workspace,
                outcomes,
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert_eq!(seen.unconsumed, 0);
        assert_eq!(seen.requests, 3);
        assert!(seen.prompts.is_empty());
        denied(&seen, "Human-only approval is unavailable");
        assert!(!host.workspace.join("ran.txt").exists());
        let session = seen.outcome.session_id;
        let (_, view) = host.reopen(session).await;
        let tool = view.runs[0].tools.last().unwrap();
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert!(!verdict.approved);
        assert_eq!(verdict.source, PermissionSource::UnavailableApproval);
        assert_eq!(
            tool.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Denied
        );
    }
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
    denied(&seen, "needs fresh human approval");
    assert!(!host.workspace.join("ran.txt").exists());
}

#[tokio::test]
async fn live_session_approval_reuses_identical_intents_without_writing_old_rules() {
    let host = Host::new(None);
    let call = unguarded(SCRIPT);
    let seen = host
        .run(
            &host.workspace,
            vec![
                arany::test_support::scenario::plan_outcome(plan(&call)),
                tool_outcome(call.clone()),
                tool_outcome(call.clone()),
                finish("answer"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowForSession],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    assert!(host.rules().is_none());
    let verdict = last_tool(&seen).permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.source, PermissionSource::SessionApproval);
    assert!(verdict.host.as_ref().unwrap().source_intent.is_some());
    assert_eq!(seen.outcome.run.unguarded_tools(), 2);
    let (_, view) = host.reopen(seen.outcome.session_id).await;
    assert_eq!(view.runs[0].unguarded_tools(), 2);
}

#[tokio::test]
async fn primary_inspects_an_absolute_host_target_without_guarded_programs_or_failure() {
    let host = Host::new(None);
    let outside = host._temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("visible.txt"), "fixture").unwrap();
    let config_path = host.state.join("tools.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["commands"] = json!([]);
    private_file(&config_path, &serde_json::to_vec(&config).unwrap());
    let call = ToolCall::CommandUnguarded {
        program: "find".into(),
        args: vec![outside.display().to_string(), "-type".into(), "f".into()],
        cwd: String::new(),
        justification: "Inspect the exact host directory the user requested".into(),
    };
    let seen = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(call),
                finish("Only the requested directory was inspected"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    let observation = last_tool(&seen).observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    assert!(observation.output.contains("visible.txt"));
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Unguarded(_))
    ));
    let (events, _) = host.reopen(seen.outcome.session_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn session_host_authority_survives_turns_but_revocation_requires_a_human() {
    let mut host = Host::new(None);
    let call = unguarded("printf inspected");
    let outcomes = || vec![tool_outcome(call.clone()), finish("done")];
    let first = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowForSession],
        )
        .await;
    host.session = Some(first.outcome.session_id);
    let second = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(second.prompts.is_empty());
    assert_eq!(
        last_tool(&second)
            .permission_verdict
            .as_ref()
            .unwrap()
            .source,
        PermissionSource::SessionApproval
    );
    host.engine.revoke_host_approvals().unwrap();
    let third = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(third.prompts.len(), 1);
    assert_eq!(
        last_tool(&third)
            .permission_verdict
            .as_ref()
            .unwrap()
            .source,
        PermissionSource::Human
    );
    let (_, replay) = host.reopen(third.outcome.session_id).await;
    assert_eq!(replay.runs.len(), 3);
}

#[tokio::test]
async fn changed_permission_settings_require_explicit_readmission() {
    let host = Host::new(None);
    let call = unguarded("printf inspected");
    let outcomes = || vec![tool_outcome(call.clone()), finish("done")];
    host.run(
        &host.workspace,
        outcomes(),
        Some(ApprovalMode::Request),
        vec![ApprovalReply::AllowOnce],
    )
    .await;
    private_file(
        &host.state.join("permissions.json"),
        br#"{"permissions":{"allow":["Bash"]}}"#,
    );
    let denied_run = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(denied_run.prompts.is_empty());
    denied(&denied_run, "use /permissions to re-admit");
    host.engine.readmit_host_permissions().unwrap();
    let admitted = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(admitted.prompts.len(), 1);
    assert_eq!(
        last_tool(&admitted)
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
}

#[tokio::test]
async fn every_privilege_tool_is_handed_off_without_approval_or_dispatch() {
    for program in [
        "sudo",
        "sudoedit",
        "doas",
        "su",
        "pkexec",
        "run0",
        "runuser",
        "chroot",
        "systemd-run",
        "machinectl",
        "pfexec",
        "ksu",
        "dzdo",
        "authopen",
        "/usr/libexec/authopen",
        "/usr/bin/sudo",
    ] {
        for mode in ["default", "auto", "acceptEdits", "dontAsk", "plan"] {
            let host = Host::new(Some(
                &json!({"permissions":{"defaultMode":mode,"allow":["Bash"]}}),
            ));
            let call = ToolCall::CommandUnguarded {
                program: program.into(),
                args: vec!["echo".into(), "canary".into()],
                cwd: String::new(),
                justification: "Inspect host".into(),
            };
            let seen = host
                .run(
                    &host.workspace,
                    vec![
                        tool_outcome(call),
                        finish("Run the displayed command by hand"),
                    ],
                    Some(ApprovalMode::Request),
                    vec![],
                )
                .await;
            assert!(seen.prompts.is_empty(), "{program} {mode}");
            denied(
                &seen,
                "Arany cannot run privilege-escalation commands. Please run this command by hand:",
            );
            assert!(
                last_tool(&seen)
                    .observation
                    .as_ref()
                    .unwrap()
                    .output
                    .contains(program)
            );
            let (_, view) = host.reopen(seen.outcome.session_id).await;
            assert_eq!(view.runs[0].unguarded_tools(), 0);
        }
    }
}

#[tokio::test]
async fn unsupported_host_prefix_reply_does_not_downgrade_to_once() {
    let host = Host::new(None);
    let seen = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(unguarded("printf should-not-run")),
                finish("refused"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowPrefixInFolder],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    denied(&seen, "Approval was not granted");
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
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

#[tokio::test]
async fn ignored_stop_hook_does_not_offer_host_execution_or_force_continuation() {
    let host = Host::new(Some(
        &json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mkdir .ssh"}]}]}}),
    ));
    let seen = host
        .run(
            &host.workspace,
            vec![finish("done")],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert_eq!(seen.requests, 1);
    assert!(seen.prompts.is_empty());
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
    let (_, view) = host.reopen(seen.outcome.session_id).await;
    assert!(view.runs[0].lifecycle_hooks.iter().all(|hook| matches!(
        hook.outcome.as_ref().unwrap().result,
        arany::permissions::HookResult::Ignored { .. }
    )));
}

#[tokio::test]
async fn ignored_pre_compact_hook_does_not_block_for_a_host_offer() {
    const HOOK: &str = "mkdir .ssh";
    let host = Host::new(Some(
        &json!({"hooks": {"PreCompact": [{"hooks": [{"type": "command", "command": HOOK}]}]}}),
    ));
    let seen = host
        .run(&host.workspace, vec![finish("done")], None, vec![])
        .await;
    let session = seen.outcome.session_id;
    let providers = arany::AdmittedProviders::single(Arc::new(
        ScriptedProvider::new(vec![]).with_compactions(vec![Ok(arany::CompactionResponse {
            summary: "Completed host-hook scenario".into(),
            response_id: None,
            usage: arany::Usage::default(),
            wire_provenance: None,
        })]),
    ));
    assert!(!matches!(
        host.engine
            .compact_session(&providers, session, host.workspace.clone())
            .await,
        Err(arany::EngineError::CompactionBlockedByHook)
    ));
    let (events, _) = host.reopen(session).await;
    assert!(!events.iter().any(|event| matches!(&event.event,
        Event::ToolStarted { intent, .. } if intent.call.is_unguarded())));
}

#[test]
fn command_output_truncation_continues_integrates_and_replays_without_execution() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    for case in ["large", "grouped", "invalid-utf8", "lines"] {
        let host = Host::new(None);
        let config_path = host.state.join("tools.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["write"] = json!(true);
        private_file(&config_path, &serde_json::to_vec(&config).unwrap());
        for name in ["a", "b", "c", "d", "e", "f"] {
            std::fs::write(
                host.workspace.join("notes").join(name),
                "line\n".repeat(2400),
            )
            .unwrap();
        }
        let script = match case {
            "large" => {
                "printf HEAD; printf '%1048568s' ''; printf TAIL; printf changed > notes/made"
            }
            "grouped" => "cat notes/a notes/b notes/c notes/d notes/e notes/f",
            "invalid-utf8" => "printf '\\377\\n'",
            "lines" => "for i in $(seq 1 20000); do echo line $i; done",
            _ => unreachable!(),
        };
        let call = guarded(script);
        let seen = host
            .run(
                &host.workspace,
                vec![
                    arany::test_support::scenario::plan_outcome(plan(&call)),
                    tool_outcome(call.clone()),
                    finish("continued after output"),
                ],
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert_eq!(
            seen.outcome.run.status,
            RunStatus::Finished,
            "{case}: {}",
            tool_report(
                seen.outcome
                    .run
                    .tools
                    .first()
                    .and_then(|t| t.observation.as_ref())
            )
        );
        assert_eq!(seen.unconsumed, 0);
        let observation = seen.outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
        let result: Value = serde_json::from_str(&observation.output).unwrap();
        assert_eq!(result["exit_code"], 0);
        let stdout = result["stdout"].as_str().unwrap();
        match case {
            "large" => {
                assert_eq!(result["stdout_total"], json!({"bytes":1048576,"lines":1}));
                assert!(stdout.starts_with("HEAD") && stdout.ends_with("TAIL"));
                assert_eq!(
                    std::fs::read(host.workspace.join("notes/made")).unwrap(),
                    b"changed"
                );
                assert_eq!(
                    seen.outcome.run.tools.len(),
                    2,
                    "ordinary changeset integration follows the truncated observation"
                );
            }
            "grouped" => {
                assert_eq!(result["stdout_total"], json!({"bytes":72000,"lines":14400}));
                assert!(stdout.starts_with("line\n") && stdout.ends_with("line\n"));
            }
            "lines" => {
                assert_eq!(result["stdout_total"], json!({"bytes":208894,"lines":20000}));
                assert!(stdout.starts_with("line 1\nline 2\n"));
                assert!(stdout.ends_with("line 19999\nline 20000\n"));
                assert!(stdout.contains("\n…["));
                assert!(
                    observation.output.len() <= 64 * 1024,
                    "{}",
                    observation.output.len()
                );
            }
            "invalid-utf8" => {
                assert_eq!(stdout, "\u{fffd}\n");
                assert_eq!(result["stdout_total"], json!({"bytes":2,"lines":1}));
            }
            _ => unreachable!(),
        }
        if case != "invalid-utf8" {
            assert!(stdout.contains(" lines omitted; "));
        }
        let next = seen.calls.last().unwrap().input.tools.as_ref().unwrap();
        assert_eq!(next.observations()[0].output, observation.output);
        let session = seen.outcome.session_id;
        let (_, replay) = host.reopen(session).await;
        assert_eq!(
            replay.runs[0], seen.outcome.run,
            "byte-stable closed replay: {case}"
        );
    }
    });
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native 1 GiB output drain ceiling on Linux and macOS"
)]
fn runaway_command_output_stops_at_the_drain_ceiling_and_publishes_nothing() {
    let _gate = process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let host = Host::new(None);
            let call = guarded(
                "printf must-not-publish > notes/made; while :; do printf '%65536s' ''; done",
            );
            let seen = host
                .run(
                    &host.workspace,
                    vec![
                        arany::test_support::scenario::plan_outcome(plan(&call)),
                        tool_outcome(call),
                        finish("limit observed"),
                    ],
                    Some(ApprovalMode::Request),
                    vec![],
                )
                .await;
            let observation = seen.outcome.run.tools[0].observation.as_ref().unwrap();
            assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
            assert!(
                observation.limit_cause == Some(arany::LimitCause::OutputCeiling),
                "{}",
                observation.output
            );
            assert!(!host.workspace.join("notes/made").exists());
            let session = seen.outcome.session_id;
            let (_, replay) = host.reopen(session).await;
            assert_eq!(replay.runs[0], seen.outcome.run);
        });
}

#[tokio::test]
async fn wrapped_privilege_tools_are_handed_off_before_any_approval() {
    for (program, args) in [
        ("env", vec!["sudo", "true"]),
        ("env", vec!["-S", "sudo id"]),
        ("env", vec!["--split-string=sudo id"]),
        ("sh", vec!["-c", "sudo true"]),
        ("sh", vec!["-c", "echo $(/usr/bin/sudo id)"]),
        ("python3", vec!["-c", "system('sudo true')"]),
        ("pwsh", vec!["-Command", "sudo true"]),
        ("xargs", vec!["/usr/bin/sudo", "true"]),
        ("find", vec![".", "-exec", "sudo", "true", ";"]),
        ("timeout", vec!["5", "sudo", "true"]),
        (
            "osascript",
            vec!["-e", "do shell script \"id\" with administrator privileges"],
        ),
    ] {
        let host = Host::new(None);
        let call = ToolCall::CommandUnguarded {
            program: program.into(),
            args: args.into_iter().map(str::to_owned).collect(),
            cwd: String::new(),
            justification: "Inspect host".into(),
        };
        let seen = host
            .run(
                &host.workspace,
                vec![
                    tool_outcome(call),
                    finish("Run the displayed command by hand"),
                ],
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert!(seen.prompts.is_empty(), "{program}");
        denied(&seen, "Please run this command by hand:");
        assert!(
            last_tool(&seen)
                .observation
                .as_ref()
                .unwrap()
                .attestation
                .is_none()
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn host_child_has_a_new_session_and_no_controlling_terminal() {
    let host = Host::new(None);
    let call = unguarded(
        "ps -o sess= -p $$ > session; if (exec 3<>/dev/tty) 2>/dev/null; then exit 1; fi",
    );
    let seen = host
        .run(
            &host.workspace,
            vec![tool_outcome(call), finish("Checked session")],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    let observation = last_tool(&seen).observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    let arany::Attestation::Unguarded(receipt) = observation.attestation.as_ref().unwrap() else {
        panic!("host receipt");
    };
    let session: u32 = std::fs::read_to_string(host.workspace.join("session"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(session, receipt.pid);
}

#[tokio::test]
async fn wrapped_privilege_symlink_is_handed_off_before_approval() {
    let host = Host::new(None);
    let privilege = host._temp.path().join("sudo");
    std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), &privilege).unwrap();
    std::os::unix::fs::symlink(&privilege, host.workspace.join("alias")).unwrap();
    let call = ToolCall::CommandUnguarded {
        program: "env".into(),
        args: vec!["./alias".into(), "true".into()],
        cwd: String::new(),
        justification: "Inspect host".into(),
    };
    let seen = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(call),
                finish("Run the displayed command by hand"),
            ],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(seen.prompts.is_empty());
    denied(&seen, "Please run this command by hand:");
    assert!(
        last_tool(&seen)
            .observation
            .as_ref()
            .unwrap()
            .attestation
            .is_none()
    );
}

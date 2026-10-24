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
        prefix_rule: None,
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
    uncertain_save: bool,
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
            uncertain_save: false,
            session: None,
            policy: CollaborationPolicy::Single,
        }
    }

    fn rules(&self) -> Option<Value> {
        std::fs::read(
            self.state
                .join("archive/inactive-host-commands/unguarded-commands.json"),
        )
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
        if input.session_id.is_some() {
            input.title = None;
        }
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
                            tokio::time::pause();
                            tokio::time::advance(Duration::from_secs(3600)).await;
                            tokio::time::resume();
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
                self.uncertain_save
                    || observation
                        .is_none_or(|o| o.disposition != arany::ToolDisposition::Uncertain),
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

fn assert_admission_history(run: &arany::RunView) {
    let tool = run.tools.last().unwrap();
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(tool.model_denial_label(), None);
    let output = serde_json::to_string(&observation.output).unwrap();
    assert!(arany::test_support::history_tool_block(run).contains(&output));
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
    assert!(seen.intents[0].host_command.is_some());
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

fn assert_unmet_host_criterion(seen: &Seen) {
    assert_eq!(
        seen.unconsumed, 0,
        "the explanatory Provider response is consumed"
    );
    assert_eq!(seen.outcome.run.status, RunStatus::Failed);
    assert_eq!(
        seen.outcome.run.failure,
        Some(arany::RunFailure::CriterionUnmet)
    );
    assert!(seen.outcome.run.assistant_message.is_none());
    let observations = seen
        .calls
        .last()
        .unwrap()
        .input
        .tools
        .as_ref()
        .unwrap()
        .observations();
    let denied = last_tool(seen).observation.as_ref().unwrap();
    assert_eq!(observations.last().copied(), Some(denied));
    assert_eq!(
        denied.approval_cause,
        Some(arany::ApprovalCause::ChannelUnavailable)
    );
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
                    finish("answer"),
                ],
                None,
                vec![],
            )
            .await;
        denied(&seen, "channel_unavailable");
        assert_eq!(seen.requests, 3);
        assert_unmet_host_criterion(&seen);
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
        let outcomes = script(&first, &second, vec![]);
        let seen = host
            .run(
                &host.workspace,
                outcomes,
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert_eq!(seen.unconsumed, 0);
        assert_eq!(seen.requests, 4);
        assert!(seen.prompts.is_empty());
        denied(&seen, "channel_unavailable");
        assert_unmet_host_criterion(&seen);
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
    denied(&seen, "channel_unavailable");
    assert_unmet_host_criterion(&seen);
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
async fn host_interpreter_refusal_distinguishes_a_pin_from_admission() {
    for (program, configured) in [("sh", false), ("sh", true), ("env", false), ("env", true)] {
        let host = Host::new(None);
        let config_path = host.state.join("tools.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        if configured {
            config["commands"][0]["interpreter"] = json!(false);
        } else {
            config["commands"] = json!([]);
        }
        private_file(&config_path, &serde_json::to_vec(&config).unwrap());
        let call = if program == "sh" {
            unguarded("printf inspected")
        } else {
            ToolCall::CommandUnguarded {
                program: program.into(),
                args: vec!["sh".into(), "-c".into(), "printf inspected".into()],
                cwd: String::new(),
                justification: "Inspect through an explicit interpreter".into(),
                prefix_rule: None,
            }
        };
        let seen = host
            .run(
                &host.workspace,
                vec![tool_outcome(call), finish("refused")],
                Some(ApprovalMode::Request),
                vec![],
            )
            .await;
        assert!(seen.prompts.is_empty());
        assert!(last_tool(&seen).intent.host_command.is_some());
        denied(
            &seen,
            "Host interpreter requires a matching tools.json command with interpreter: true",
        );
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(view.runs[0].unguarded_tools(), 0);
        let tool = view.runs[0].tools.last().unwrap();
        assert!(tool.intent.host_command.is_some());
        assert!(tool.observation.as_ref().unwrap().attestation.is_none());
        assert!(!tool.permission_verdict.as_ref().unwrap().approved);
        assert_admission_history(&view.runs[0]);
    }
}

#[tokio::test]
async fn non_reusable_host_requests_offer_once_only() {
    for rule in [false, true] {
        let host = Host::new(rule.then_some(&json!({"permissions":{"ask":["command_unguarded"]}})));
        let call = if rule {
            unguarded("printf inspected")
        } else {
            ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf inspected".into(),
                    host.state.to_string_lossy().into_owned(),
                ],
                cwd: String::new(),
                justification: "Inspect a user-requested state target".into(),
                prefix_rule: None,
            }
        };
        let seen = host
            .run(
                &host.workspace,
                vec![tool_outcome(call), finish("refused")],
                Some(ApprovalMode::Request),
                vec![ApprovalReply::Deny],
            )
            .await;
        assert_eq!(seen.prompts.len(), 1);
        assert!(!seen.prompts[0].remember);
        denied(&seen, "user_declined");
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(view.runs[0].unguarded_tools(), 0);
    }
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
        prefix_rule: None,
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
    private_file(&host.state.join("permissions.json"), b"invalid");
    assert!(host.engine.readmit_host_permissions().is_err());
    std::fs::remove_file(host.state.join("permissions.json")).unwrap();
    let still_denied = host
        .run(
            &host.workspace,
            outcomes(),
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    denied(&still_denied, "use /permissions to re-admit");
    assert!(still_denied.prompts.is_empty());
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
async fn live_settings_drift_refuses_before_prompt_and_rechecks_after_approval() {
    for before_prompt in [true, false] {
        let host = Host::new(None);
        let path = host.state.join("permissions.json");
        let edited = path.clone();
        let provider = Arc::new(
            ScriptedProvider::new(vec![tool_outcome(unguarded(SCRIPT)), finish("done")])
                .with_before_call(0, move || {
                    if before_prompt {
                        private_file(&edited, b"{}");
                    }
                }),
        );
        let (approvals, mut inbox) = arany::ToolApprovals::new(ApprovalMode::Request);
        let mut input = request(&host.workspace, "Inspect a command under changed settings");
        input.include_paths.clear();
        input.goal.budget = Some(budget());
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        };
        let mut prompts = 0;
        let outcome = {
            let running = run_with(&host.engine, &provider, input, controls);
            tokio::pin!(running);
            loop {
                tokio::select! {
                    result = &mut running => break result.unwrap(),
                    Some(approval) = inbox.next() => {
                        assert!(approval.unguarded.is_some());
                        prompts += 1;
                        private_file(&path, b"{}");
                        approval.decide(ApprovalReply::AllowOnce);
                    }
                }
            }
        };
        assert_eq!(prompts, usize::from(!before_prompt));
        assert_eq!(outcome.run.unguarded_tools(), 0);
        assert!(!host.workspace.join("ran.txt").exists());
        let (_, view) = host.reopen(outcome.session_id).await;
        let tool = view.runs[0].tools.last().unwrap();
        let observation = tool.observation.as_ref().unwrap();
        assert!(observation.attestation.is_none());
        if before_prompt {
            assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
            assert!(
                observation
                    .output
                    .contains("permission settings changed outside admission")
            );
            assert!(!tool.permission_verdict.as_ref().unwrap().approved);
        } else {
            assert_eq!(observation.disposition, arany::ToolDisposition::Failed);
            assert!(tool.permission_verdict.as_ref().unwrap().approved);
        }
        assert_admission_history(&view.runs[0]);
    }
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
                prefix_rule: None,
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
            let expected = format!(
                "Arany cannot run privilege-escalation commands. Please run this command by hand:\nargv (JSON): {}\nWorkspace-relative cwd (JSON): \"\"\nNothing ran. Do not repackage this command through a wrapper, shell or child.",
                json!([program, "echo", "canary"]),
            );
            assert_eq!(
                last_tool(&seen).observation.as_ref().unwrap().output,
                expected
            );
            let (_, view) = host.reopen(seen.outcome.session_id).await;
            assert_eq!(view.runs[0].unguarded_tools(), 0);
            assert_admission_history(&view.runs[0]);
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
    denied(&seen, "channel_unavailable");
    assert_eq!(
        seen.outcome.run.assistant_message.as_deref(),
        Some("refused")
    );
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
    let session = seen.outcome.session_id;
    let (_, view) = host.reopen(session).await;
    let tool = view.runs[0].tools.last().unwrap();
    assert_eq!(
        tool.permission_verdict.as_ref().unwrap().source,
        PermissionSource::UnavailableApproval
    );
    assert_eq!(
        tool.observation.as_ref().unwrap().approval_cause,
        Some(arany::ApprovalCause::ChannelUnavailable)
    );
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
    denied(&seen, "user_declined");
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
    let compacted = host
        .engine
        .compact_session(&providers, session, host.workspace.clone())
        .await
        .expect("ignored PreCompact hook permits compaction");
    assert!(matches!(
        compacted.status,
        arany::CompactionStatus::Succeeded { .. }
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
            prefix_rule: None,
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
        prefix_rule: None,
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

fn prefix_call() -> ToolCall {
    ToolCall::CommandUnguarded {
        program: "printf".into(),
        args: vec!["host prefix %s".into(), "first".into()],
        cwd: String::new(),
        justification: "Inspect a bounded synthetic host value".into(),
        prefix_rule: Some(vec!["printf".into(), "host prefix %s".into()]),
    }
}

#[tokio::test]
async fn host_prefix_save_reuses_next_run_and_revocation_restores_the_prompt() {
    let host = Host::new(None);
    let call = prefix_call();
    let first = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(call.clone()),
                tool_outcome(call.clone()),
                finish("done"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowPrefixInFolder, ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(
        first.prompts.len(),
        2,
        "a saved rule cannot change its Run snapshot"
    );
    assert!(first.prompts[0].prefix);
    assert_eq!(first.outcome.run.unguarded_tools(), 2);
    let mut next = call.clone();
    let ToolCall::CommandUnguarded { args, .. } = &mut next else {
        unreachable!()
    };
    args[1] = "second".into();
    let second = host
        .run(
            &host.workspace,
            vec![tool_outcome(next), finish("done")],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(second.prompts.is_empty());
    let tool = last_tool(&second);
    assert_eq!(
        tool.permission_verdict.as_ref().unwrap().source,
        PermissionSource::HostPrefixRule
    );
    assert!(
        tool.observation
            .as_ref()
            .unwrap()
            .output
            .contains("host prefix second")
    );
    let state = StateRoot::open_existing(&host.state).unwrap();
    let settings = arany::permissions::PermissionSettings::load(&state, &host.workspace).unwrap();
    assert_eq!(settings.host_prefixes().len(), 1);
    settings
        .save_host_edit(
            &state,
            &host.workspace,
            arany::permissions::HostPrefixEdit::Remove { index: 0 },
        )
        .unwrap();
    host.engine.readmit_host_permissions().unwrap();
    let third = host
        .run(
            &host.workspace,
            vec![tool_outcome(call), finish("refused")],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::Deny],
        )
        .await;
    denied(&third, "user_declined");
    let (events, view) = host.reopen(second.outcome.session_id).await;
    assert_eq!(view.runs[0].unguarded_tools(), 1);
    assert!(events.iter().any(|event| matches!(&event.event, Event::ToolPermissionDecided { verdict, .. }
        if verdict.source == PermissionSource::HostPrefixRule && verdict.host.as_ref().is_some_and(|host| host.rule.is_some()))));
    for prefix in 1..=events.len() {
        SessionView::replay(second.outcome.session_id, &events[..prefix]).unwrap();
    }
    for mutation in [
        "missing", "prefix", "cwd", "deny", "mode", "session", "epoch", "typed", "snapshot",
    ] {
        let mut forged = events.clone();
        let verdict = forged
            .iter_mut()
            .find_map(|event| match &mut event.event {
                Event::ToolPermissionDecided { verdict, .. }
                    if verdict.source == PermissionSource::HostPrefixRule =>
                {
                    Some(verdict)
                }
                _ => None,
            })
            .unwrap();
        match mutation {
            "missing" => verdict.host.as_mut().unwrap().rule = None,
            "prefix" => {
                verdict.host.as_mut().unwrap().rule.as_mut().unwrap().prefix[0] = "other".into()
            }
            "cwd" => verdict.host.as_mut().unwrap().rule.as_mut().unwrap().cwd = "notes".into(),
            "deny" => {
                verdict
                    .host
                    .as_mut()
                    .unwrap()
                    .rule
                    .as_mut()
                    .unwrap()
                    .decision = Decision::Deny
            }
            "mode" => verdict.mode = Some(arany::permissions::PermissionMode::Auto),
            "session" => verdict.host.as_mut().unwrap().session = true,
            "epoch" => verdict.host.as_mut().unwrap().epoch = uuid::Uuid::now_v7(),
            "typed" => verdict.source = PermissionSource::TypedPrefixRule,
            "snapshot" => {}
            _ => unreachable!(),
        }
        if mutation == "snapshot" {
            for event in &mut forged {
                if let Event::RunStarted { config, .. } = &mut event.event {
                    config.tool_policy.as_mut().unwrap().host_prefixes.clear();
                }
            }
        }
        assert!(
            SessionView::replay(second.outcome.session_id, &forged).is_err(),
            "{mutation}"
        );
    }
}

#[tokio::test]
async fn host_prefix_modes_channels_and_explicit_rules_never_widen_authority() {
    for (mode, terminal, channel, restriction, reuse) in [
        ("default", ApprovalMode::Request, true, None, true),
        ("dontAsk", ApprovalMode::Request, true, None, true),
        ("dontAsk", ApprovalMode::Request, true, Some("ask"), false),
        ("auto", ApprovalMode::Request, true, None, false),
        ("acceptEdits", ApprovalMode::Request, true, None, false),
        ("default", ApprovalMode::Auto, true, None, false),
        ("default", ApprovalMode::AutoEdits, true, None, false),
        ("default", ApprovalMode::Request, false, None, false),
        ("default", ApprovalMode::Request, true, Some("ask"), false),
        ("default", ApprovalMode::Request, true, Some("deny"), false),
        ("plan", ApprovalMode::Request, true, None, false),
    ] {
        let host = Host::new(None);
        let call = prefix_call();
        let first = host
            .run(
                &host.workspace,
                vec![tool_outcome(call.clone()), finish("done")],
                Some(ApprovalMode::Request),
                vec![ApprovalReply::AllowPrefixInFolder],
            )
            .await;
        assert_eq!(first.outcome.run.unguarded_tools(), 1);
        let file = host.state.join("permissions.json");
        let mut value: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        value["permissions"]["defaultMode"] = json!(mode);
        if let Some(restriction) = restriction {
            value["permissions"][restriction] = json!(["Bash"]);
        }
        private_file(&file, &serde_json::to_vec(&value).unwrap());
        host.engine.readmit_host_permissions().unwrap();
        let prompts =
            channel && !reuse && restriction != Some("deny") && !matches!(mode, "plan" | "dontAsk");
        let seen = host
            .run(
                &host.workspace,
                vec![tool_outcome(call), finish("done")],
                channel.then_some(terminal),
                if prompts {
                    vec![ApprovalReply::AllowOnce]
                } else {
                    vec![]
                },
            )
            .await;
        assert_eq!(
            seen.prompts.len(),
            usize::from(prompts),
            "{mode} {terminal:?} {channel} {restriction:?}"
        );
        assert_eq!(
            seen.outcome.run.unguarded_tools(),
            usize::from(reuse || prompts)
        );
        let source = last_tool(&seen).permission_verdict.as_ref().unwrap().source;
        assert_eq!(source == PermissionSource::HostPrefixRule, reuse);
        let (events, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(
            view.runs[0].unguarded_tools(),
            usize::from(reuse || prompts)
        );
        if mode == "dontAsk" && restriction == Some("ask") {
            assert_host_ask_denial(&seen, &events);
        }
    }
}

fn assert_host_ask_denial(seen: &Seen, events: &[arany::EventEnvelope]) {
    let verdict = last_tool(seen).permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.decision, Decision::Deny);
    assert_eq!(verdict.basis, PermissionSource::Rule);
    assert!(!verdict.approved && verdict.host.is_none());
    let observation = last_tool(seen).observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
    assert!(observation.attestation.is_none());
    for prefix in 1..=events.len() {
        SessionView::replay(seen.outcome.session_id, &events[..prefix]).unwrap();
    }
}

#[tokio::test]
async fn host_prefix_state_targets_and_unadmitted_settings_changes_dispatch_nothing() {
    for drift in [false, true] {
        let host = Host::new(None);
        let mut call = prefix_call();
        host.run(
            &host.workspace,
            vec![tool_outcome(call.clone()), finish("done")],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowPrefixInFolder],
        )
        .await;
        if drift {
            let file = host.state.join("permissions.json");
            let mut bytes = std::fs::read(&file).unwrap();
            bytes.push(b' ');
            private_file(&file, &bytes);
        } else {
            let ToolCall::CommandUnguarded { args, .. } = &mut call else {
                unreachable!()
            };
            args[1] = host.state.display().to_string();
        }
        let seen = host
            .run(
                &host.workspace,
                vec![tool_outcome(call), finish("refused")],
                Some(ApprovalMode::Request),
                if drift {
                    vec![]
                } else {
                    vec![ApprovalReply::Deny]
                },
            )
            .await;
        assert_eq!(seen.outcome.run.unguarded_tools(), 0);
        assert!(
            !last_tool(&seen)
                .permission_verdict
                .as_ref()
                .unwrap()
                .approved
        );
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(view.runs[0].unguarded_tools(), 0);
    }
}

#[tokio::test]
async fn host_prefix_overflow_and_unconfirmed_save_never_spawn() {
    for uncertain in [false, true] {
        let mut host = if uncertain {
            Host::new(None)
        } else {
            Host::new(Some(
                &json!({"permissions":{"allow":vec!["Read"; arany::permissions::MAX_RULES]}}),
            ))
        };
        host.uncertain_save = uncertain;
        if uncertain {
            arany::FaultPlan::fail_record_sync(host.state.clone(), "permissions.json");
        }
        let seen = host
            .run(
                &host.workspace,
                vec![tool_outcome(prefix_call()), finish("refused")],
                Some(ApprovalMode::Request),
                vec![ApprovalReply::AllowPrefixInFolder],
            )
            .await;
        let tool = last_tool(&seen);
        assert!(
            tool.permission_verdict
                .as_ref()
                .unwrap()
                .host
                .as_ref()
                .unwrap()
                .rule
                .is_some(),
            "the human's save action is committed before saving"
        );
        let observation = tool.observation.as_ref().unwrap();
        assert!(
            observation.attestation.is_none(),
            "the host program never ran"
        );
        assert_eq!(
            observation.disposition,
            if uncertain {
                arany::ToolDisposition::Uncertain
            } else {
                arany::ToolDisposition::Denied
            }
        );
        assert_eq!(seen.requests, if uncertain { 1 } else { 2 });
        let (_, view) = host.reopen(seen.outcome.session_id).await;
        assert_eq!(
            view.runs[0]
                .tools
                .last()
                .unwrap()
                .observation
                .as_ref()
                .unwrap()
                .disposition,
            observation.disposition
        );
    }
}

#[tokio::test]
async fn permissions_can_explicitly_create_one_token_but_model_proposals_cannot() {
    let host = Host::new(None);
    let call = prefix_call();
    let first = host
        .run(
            &host.workspace,
            vec![tool_outcome(call.clone()), finish("done")],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    let state = StateRoot::open_existing(&host.state).unwrap();
    let settings = arany::permissions::PermissionSettings::load(&state, &host.workspace).unwrap();
    let rule = arany::permissions::HostPrefixRule::from_intent(&first.intents[0], 1).unwrap();
    let saved = settings
        .save_host_edit(
            &state,
            &host.workspace,
            arany::permissions::HostPrefixEdit::Insert(Box::new(rule)),
        )
        .unwrap();
    assert_eq!(saved.host_prefixes()[0].prefix, ["printf"]);
    assert!(matches!(
        settings.save_host_edit(
            &state,
            &host.workspace,
            arany::permissions::HostPrefixEdit::Remove { index: 0 }
        ),
        Err(arany::permissions::SettingsError::Conflict)
    ));
    host.engine.readmit_host_permissions().unwrap();
    let seen = host
        .run(
            &host.workspace,
            vec![tool_outcome(call), finish("done")],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert_eq!(seen.outcome.run.unguarded_tools(), 1);
    let mut bad = prefix_call();
    let ToolCall::CommandUnguarded { prefix_rule, .. } = &mut bad else {
        unreachable!()
    };
    *prefix_rule = Some(vec!["printf".into()]);
    let seen = host
        .run(
            &host.workspace,
            vec![tool_outcome(bad), finish("refused")],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(seen.prompts.is_empty());
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
    host.reopen(seen.outcome.session_id).await;
}

#[tokio::test]
async fn remembered_host_rules_never_become_child_authority() {
    let mut host = Host::new(None);
    let call = prefix_call();
    host.run(
        &host.workspace,
        vec![tool_outcome(call.clone()), finish("done")],
        Some(ApprovalMode::Request),
        vec![ApprovalReply::AllowPrefixInFolder],
    )
    .await;
    host.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let seen = host
        .run(
            &host.workspace,
            vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Inspect a synthetic value".into(),
                        profile: None,
                    }],
                }),
                tool_outcome(call),
                finish("report"),
                finish("answer"),
            ],
            Some(ApprovalMode::Request),
            vec![],
        )
        .await;
    assert!(seen.prompts.is_empty());
    assert_eq!(seen.outcome.run.unguarded_tools(), 0);
    let (_, view) = host.reopen(seen.outcome.session_id).await;
    let child = &view.runs[0].agents[1];
    assert_eq!(
        child.grant,
        Some(arany::ChildGrant::ReadOnly {
            unguarded: arany::UnguardedGrant::Denied
        })
    );
    assert!(child.tools.iter().all(|tool| !tool.ran_unguarded()));
}

#[tokio::test]
async fn a_host_write_to_settings_creates_no_new_authority_and_blocks_reuse() {
    let host = Host::new(None);
    let call = prefix_call();
    host.run(
        &host.workspace,
        vec![tool_outcome(call.clone()), finish("done")],
        Some(ApprovalMode::Request),
        vec![ApprovalReply::AllowPrefixInFolder],
    )
    .await;
    let settings = host.state.join("permissions.json");
    let path = serde_json::to_string(settings.to_str().unwrap()).unwrap();
    let seen = host
        .run(
            &host.workspace,
            vec![
                tool_outcome(unguarded(&format!("printf ' ' >> {path}"))),
                tool_outcome(call),
                finish("settings changed"),
            ],
            Some(ApprovalMode::Request),
            vec![ApprovalReply::AllowOnce],
        )
        .await;
    assert_eq!(seen.prompts.len(), 1);
    assert_eq!(seen.outcome.run.unguarded_tools(), 1);
    let verdict = last_tool(&seen).permission_verdict.as_ref().unwrap();
    assert!(!verdict.approved && verdict.host.is_none());
    let (events, view) = host.reopen(seen.outcome.session_id).await;
    assert_eq!(view.runs[0].unguarded_tools(), 1);
    assert_eq!(events.iter().filter(|event| matches!(&event.event, Event::ToolPermissionDecided { verdict, .. } if verdict.approved)).count(), 1);
}

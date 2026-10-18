use super::*;
use arany::test_support::goals::{ForegroundWorkerProvider, assert_action_bounds, replayed};
use arany::worker::{EffectTarget, WorkerDisposition, WorkerIntegrationOutcome};
use arany_cli::presentation::{Output, render_exec, render_session};
use arany_cli::protocol::EventLine;
use std::os::unix::fs::PermissionsExt;

struct WorkerObservationBundle {
    exit: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    events: Vec<arany::EventEnvelope>,
    view: SessionView,
}

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

async fn closed_bundle(
    state: &Path,
    session: SessionId,
    workspace: &Path,
) -> WorkerObservationBundle {
    use process::BoundedOutput;
    let (events, view) = replayed(state, session).await;
    let show = |output| {
        std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(workspace)
            .args(["show", "--state-dir"])
            .arg(state)
            .args(["--output", output])
            .arg(session.to_string())
            .bounded_output_for(Duration::from_secs(30), 4 * 1024 * 1024)
            .unwrap()
    };
    let text = show("text");
    assert!(text.status.success());
    assert!(text.stderr.is_empty());
    assert_eq!(
        text.stdout,
        render_session(&view, &events, Output::Text).as_bytes()
    );
    let jsonl = show("jsonl");
    assert!(jsonl.status.success());
    assert!(jsonl.stderr.is_empty());
    let actual: Vec<EventLine> = String::from_utf8(jsonl.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        actual,
        events.iter().map(EventLine::from).collect::<Vec<_>>()
    );
    WorkerObservationBundle {
        exit: text.status.code(),
        stdout: text.stdout,
        stderr: text.stderr,
        events,
        view,
    }
}

async fn worker_case(case: &'static str) {
    arany::test_support::isolate_account_root();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/a"), "original a").unwrap();
    std::fs::write(workspace.join("notes/b"), "original b").unwrap();
    let canary = "WORKER_UNDISCLOSED_SOURCE_019";
    std::fs::write(workspace.join("private-secret"), canary).unwrap();
    std::fs::create_dir(workspace.join(".git")).unwrap();
    let git = b"[core]\nfsmonitor = hostile-never-executed\n";
    std::fs::write(workspace.join(".git/config"), git).unwrap();
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#);
    std::fs::create_dir(state.join("agents")).unwrap();
    std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Omitted background must select the foreground path.
    private_file(&state.join("agents/editor.md"), b"---\nname: editor\ndescription: Edit leased notes\ntools: Read, Write, Edit\npermissionMode: acceptEdits\nisolation: worktree\n---\nEdit only the assignment.\n");
    if case == "no_channel" {
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"defaultMode":"default"}}"#,
        );
    }
    if case.starts_with("hook_") {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let hash: String = Sha256::digest(std::fs::read(&shell).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        private_file(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["notes"],"write":true,"commands":[{"name":"sh","executable":shell,"sha256":hash,"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]})).unwrap());
        let command =
            |script: &str| serde_json::json!({"type":"command","command":script,"timeout":5});
        let hooks = match case {
            "hook_write" => {
                serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command("exit 0")]}],"PostToolUse":[{"matcher":"Write","hooks":[command("echo checked >&2; exit 2")]}]})
            }
            "hook_stop_block" => {
                serde_json::json!({"SubagentStop":[{"matcher":"editor","hooks":[command("echo verify-worker >&2; exit 2")]}]})
            }
            "hook_stop" => {
                serde_json::json!({"SubagentStop":[{"matcher":"editor","hooks":[command(r#"printf '%s' '{"continue":false,"stopReason":"halt"}'"#)]}]})
            }
            "hook_write_stop" => {
                serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command(r#"printf '%s' '{"continue":false,"stopReason":"halt"}'"#)]}]})
            }
            "hook_write_update" => {
                let rewritten = serde_json::to_string(&arany::ToolCall::Write {
                    path: "notes/a".into(),
                    expected_digest: None,
                    content: "rewritten".into(),
                })
                .unwrap();
                let output = serde_json::json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":serde_json::from_str::<serde_json::Value>(&rewritten).unwrap()}});
                serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command(&format!("printf '%s' '{output}'"))]}]})
            }
            "hook_uncertain" => {
                serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command("kill -9 $PPID; sleep 5")]}]})
            }
            "hook_post_stop" => {
                serde_json::json!({"PostToolUse":[{"matcher":"Edit","hooks":[command(r#"printf '%s' '{"continue":false,"stopReason":"halt"}'"#)]}]})
            }
            "hook_handover_post" => {
                serde_json::json!({"PostToolUse":[{"matcher":"Edit","hooks":[command("echo published >&2; exit 2")]}]})
            }
            _ => {
                serde_json::json!({"PreToolUse":[{"matcher":"Edit","hooks":[command(r#"IFS= read -r input; case "$input" in *'"files":["notes/a","notes/b"]'*) echo sealed-denial >&2; exit 2;; *) exit 0;; esac"#)]}]})
            }
        };
        private_file(
            &state.join("permissions.json"),
            &serde_json::to_vec(&serde_json::json!({"hooks":hooks})).unwrap(),
        );
    }
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (engine, session) = if case == "headroom" {
        let id = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        engine.close().await.unwrap();
        pad_worker_session(&state, id);
        (
            engine_support::open(
                StateRoot::open_existing(&state).unwrap(),
                EngineConfig {
                    guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                    ..Default::default()
                },
            )
            .unwrap(),
            Some(id),
        )
    } else {
        (engine, None)
    };
    let budget = arany::GoalBudget {
        max_provider_calls: Some(if case == "calls" { 4 } else { 32 }),
        max_tokens: (case == "tokens").then_some(12),
        ..Default::default()
    };
    let provider = Arc::new(ForegroundWorkerProvider::new(&workspace, case, budget));
    let mut input = request(&workspace, "Edit notes through one foreground worker");
    input.include_paths.clear();
    input.session_id = session;
    if session.is_some() {
        input.title = None;
    }
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    input.goal = arany::GoalRequest::new(budget, arany::GoalPhase::Implementation);
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: (case != "no_channel").then_some(approvals),
        ..Default::default()
    };
    let mut running = engine
        .submit(AdmittedProviders::single(provider.clone()), input, controls)
        .unwrap()
        .result;
    let mut injected = false;
    let result = loop {
        tokio::select! {
            result = &mut running => break result,
            Some(approval) = inbox.next(), if case != "no_channel" => {
                let private = matches!(approval.intent.target, EffectTarget::WorkerView { .. });
                if !injected && (case == "private_loss" && private || case == "publication_loss" && !private) {
                    arany::FaultPlan::lose_guard_receipt(approval.intent.id); injected = true;
                }
                approval.decide(!(case == "denial" && private || case == "integration_denial" && !private));
            }
        }
    };
    drop(running);
    let violations = provider.violations.lock().unwrap().clone();
    assert!(violations.is_empty(), "{case}: {violations:?}");
    let calls = provider.requests.lock().unwrap().clone();
    let session = calls
        .first()
        .unwrap_or_else(|| panic!("{case}: no provider request; outcome {result:?}"))
        .session_id;
    engine.close().await.unwrap();
    let bundle = closed_bundle(&state, session, &workspace).await;
    assert_eq!(bundle.exit, Some(0), "{case}");
    assert!(bundle.stderr.is_empty(), "{case}");
    assert_action_bounds(&bundle.events, false, false);
    let run = bundle
        .view
        .runs
        .first()
        .unwrap_or_else(|| panic!("{case}: no run in session view"));
    if matches!(case, "background" | "mixed" | "wave") {
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(calls.len(), 2, "{case}");
        assert!(run.agents.iter().all(|a| a.assignment.is_none()));
        return;
    }
    let worker = run
        .agents
        .iter()
        .find_map(|a| a.assignment.as_ref())
        .unwrap_or_else(|| panic!("{case}: worker acquired no lease; run {:?}", run.status));
    let child_calls = calls
        .iter()
        .filter(|r| r.phase == AgentPhase::ChildWork)
        .count();
    let synthesis_calls = calls
        .iter()
        .filter(|r| r.phase == AgentPhase::RootSynthesis)
        .count();
    if case == "hook_uncertain" {
        assert!(
            matches!(
                result,
                Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
            ),
            "an unknown worker hook stops the Run as Uncertain: {result:?}"
        );
        assert_eq!(synthesis_calls, 0);
        assert_eq!(child_calls, 1);
        return;
    }
    assert_eq!(std::fs::read(workspace.join(".git/config")).unwrap(), git);
    for bytes in [
        bundle.stdout,
        format!("{calls:?}").into_bytes(),
        serde_json::to_vec(
            &bundle
                .events
                .iter()
                .map(EventLine::from)
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        std::fs::read(state.join("events.sqlite3")).unwrap(),
    ] {
        assert!(
            !bytes.windows(canary.len()).any(|b| b == canary.as_bytes()),
            "source canary remained private"
        );
    }
    if case == "publication_loss" {
        assert!(result.is_err(), "publication uncertainty is fatal");
        assert!(injected, "{case}");
        assert_eq!(synthesis_calls, 0, "{case}");
        assert!(worker.released.is_none());
        assert_eq!(
            worker.integration.as_ref().unwrap().1.outcome,
            WorkerIntegrationOutcome::Uncertain
        );
    } else {
        let result = result.expect(case);
        if case == "hook_write_update" {
            let tools = &run.agents[1].tools;
            assert!(tools.iter().all(|tool| {
                tool.observation.as_ref().unwrap().disposition == arany::ToolDisposition::Denied
            }));
            assert!(tools.iter().all(|tool| tool.hooks.len() == 1));
            assert_eq!(
                std::fs::read(workspace.join("notes/a")).unwrap(),
                b"original a"
            );
            return;
        }
        if case == "hook_post_stop" {
            assert_eq!(run.status, RunStatus::StoppedByHook);
            assert_eq!(synthesis_calls, 0);
            assert_eq!(
                std::fs::read(workspace.join("notes/a")).unwrap(),
                b"edited a"
            );
            assert_eq!(run.hook_stop_reason(), Some(Some("halt")));
            return;
        }
        assert!(result.stopped.is_none(), "{case}");
        assert!(worker.released.is_some(), "{case}");
        if matches!(
            case,
            "success" | "hook_write" | "hook_stop_block" | "hook_handover_post"
        ) {
            assert_eq!(run.status, RunStatus::Finished);
            assert_eq!(child_calls, if case == "hook_stop_block" { 12 } else { 4 });
            if case == "hook_stop_block" {
                assert_eq!(run.lifecycle_hooks.len(), 8);
            }
            if case == "hook_write" {
                let tools = &run.agents[1].tools;
                assert_eq!(tools[0].hooks.len(), 2);
                assert_eq!(tools[1].hooks.len(), 2);
                assert!(tools[..2].iter().all(|tool| {
                    tool.hooks
                        .iter()
                        .all(|hook| hook.invocation.intent.agent_run_id == run.agents[1].id)
                }));
            }
            if case == "hook_handover_post" {
                let synthesis = calls
                    .iter()
                    .find(|call| call.phase == AgentPhase::RootSynthesis)
                    .unwrap();
                assert!(
                    synthesis
                        .input
                        .tools
                        .as_ref()
                        .unwrap()
                        .steps
                        .iter()
                        .any(|step| step
                            .hook_feedback
                            .values()
                            .any(|lines| lines == &["hook feedback: published"]))
                );
            }
            assert_eq!(synthesis_calls, 2);
            assert!(worker.verification.as_ref().unwrap().1.passed);
            assert_eq!(
                worker.integration.as_ref().unwrap().1.outcome,
                WorkerIntegrationOutcome::Integrated
            );
            assert_eq!(
                render_exec(run, &bundle.events, Output::Text, true),
                "Answer:\n  Primary verified published notes\n"
            );
            assert_eq!(
                std::fs::read(workspace.join("notes/a")).unwrap(),
                b"edited a"
            );
        } else {
            assert_eq!(
                run.status,
                if matches!(case, "hook_stop" | "hook_write_stop") {
                    RunStatus::StoppedByHook
                } else {
                    RunStatus::Failed
                },
                "{case}"
            );
            if matches!(case, "hook_stop" | "hook_write_stop") {
                assert_eq!(synthesis_calls, 0);
                assert!(worker.handover.is_none());
                assert_eq!(run.hook_stop_reason(), Some(Some("halt")));
                assert_eq!(run.agents[1].status, arany::AgentStatus::StoppedByHook);
            }
            if case == "hook_handover_deny" {
                assert_eq!(
                    worker.integration.as_ref().unwrap().1.outcome,
                    WorkerIntegrationOutcome::Denied
                );
                let tool = run.tools.last().unwrap();
                assert_eq!(tool.hooks.len(), 1);
                assert!(tool.observation.as_ref().unwrap().attestation.is_none());
            }
            if case == "private_loss" {
                assert!(injected, "{case}");
                assert_eq!(synthesis_calls, 1, "{case}");
                assert_eq!(
                    worker.outcome.as_ref().unwrap().1.disposition,
                    WorkerDisposition::PrivateUncertain
                );
            }
            if matches!(case, "conflict" | "integration_denial") {
                assert_eq!(synthesis_calls, 1, "{case} reaches synthesis");
            }
            if case == "conflict" {
                assert_eq!(std::fs::read(workspace.join("notes/b")).unwrap(), b"host b");
                assert_eq!(
                    worker.integration.as_ref().unwrap().1.outcome,
                    WorkerIntegrationOutcome::Conflict
                );
            }
            if case != "unverified" {
                assert_eq!(
                    std::fs::read(workspace.join("notes/a")).unwrap(),
                    b"original a"
                );
            }
        }
    }
}

#[tokio::test]
async fn foreground_worker_guard_journey_publishes_only_the_eligible_handover_and_verifies() {
    worker_case("success").await;
    worker_case("unverified").await;
    worker_case("conflict").await;
}

#[tokio::test]
async fn foreground_worker_guard_journey_budgets_denials_and_private_vs_publication_loss() {
    for case in [
        "calls",
        "tokens",
        "denial",
        "no_channel",
        "integration_denial",
        "private_loss",
        "publication_loss",
        "headroom",
        "background",
        "mixed",
        "wave",
    ] {
        worker_case(case).await;
    }
}

#[tokio::test]
async fn foreground_worker_hook_journey_bounds_stop_blocks_and_denies_sealed_handover() {
    for case in [
        "hook_write",
        "hook_stop_block",
        "hook_handover_deny",
        "hook_handover_post",
        "hook_stop",
        "hook_write_stop",
        "hook_write_update",
        "hook_post_stop",
    ] {
        worker_case(case).await;
    }
}

#[test]
fn foreground_worker_handover_hook_fits_a_two_mebibyte_stack() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(worker_case("hook_handover_deny"));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn foreground_worker_unknown_hook_outcome_stops_the_run_as_uncertain() {
    worker_case("hook_uncertain").await;
}

// The case advances the paused clock by an hour; its own runtime keeps that offset from later cases.
#[tokio::test]
async fn foreground_worker_guard_journey_active_time_budget_closes_the_goal() {
    worker_case("time").await;
}

fn pad_worker_session(state: &Path, id: SessionId) {
    let mut connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    let mut insert = transaction.prepare("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)").unwrap();
    for sequence in 2..=9_920 {
        insert
            .execute(rusqlite::params![sequence, id.to_string()])
            .unwrap();
    }
    drop(insert);
    transaction.commit().unwrap();
}

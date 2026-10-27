use super::*;
use crate::process::BoundedOutput;
use arany::test_support::scenario::{ScriptedProvider, request};
use arany::{Engine, EngineError, Event, EventEnvelope, GitWriteStage, RunCancellation, SessionId};

pub(super) struct Recorded {
    events: Vec<EventEnvelope>,
    view: SessionView,
}

fn reservations() -> usize {
    std::fs::read_dir(StateRoot::account_path().unwrap().join("guard"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".unit"))
        .count()
}

fn input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, "Create a branch through the Guard");
    input.include_paths.clear();
    input
}

async fn drive(
    engine: &Engine,
    workspace: &Path,
    call: ToolCall,
    cancellation: RunCancellation,
) -> (Result<arany::RunOutcome, EngineError>, usize) {
    let provider = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(call),
        arany::test_support::goals::finish_with("Done"),
    ]));
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::AutoEdits);
    let running = run_with(
        engine,
        &provider,
        input(workspace),
        RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            cancellation,
            ..Default::default()
        },
    );
    tokio::pin!(running);
    let result = loop {
        tokio::select! {
            result = &mut running => break result,
            Some(approval) = inbox.next() => approval.decide(arany::ApprovalReply::AllowOnce),
        }
    };
    let calls = provider.requests.lock().unwrap().len();
    (result, calls)
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Cancel,
    Release,
    Replace,
}

async fn gated(
    engine: &Engine,
    workspace: &Path,
    stage: GitWriteStage,
    action: Action,
) -> (Result<arany::RunOutcome, EngineError>, usize) {
    let call = branch_creation(&format!("recovery-{stage:?}-{action:?}"));
    let (entered, release) = arany::FaultPlan::gate_git_write(call.clone(), stage);
    let mut release = Some(release);
    let cancellation = RunCancellation::new();
    let running = drive(engine, workspace, call, cancellation.clone());
    tokio::pin!(running);
    let (unit, helper) = tokio::select! {
        witness = entered => witness.unwrap(),
        result = &mut running => panic!("{stage:?}: finished before gate: {:?}", result.0),
    };
    assert!(unit.is_dir(), "{stage:?}: live unit root");
    assert_eq!(
        reservations(),
        2,
        "{stage:?}: both unit envelopes stay reserved through publication"
    );
    let alive = std::process::Command::new("/bin/kill")
        .args(["-0", &helper.to_string()])
        .env_clear()
        .bounded_output_for(Duration::from_secs(10), 4096)
        .unwrap();
    assert!(
        alive.status.success(),
        "{stage:?}: executor helper must remain alive until relay acknowledgement"
    );
    match action {
        Action::Cancel => {
            assert!(cancellation.cancel());
        }
        Action::Release => {
            release.take().unwrap().send(()).unwrap();
        }
        Action::Replace => {
            let original = workspace.join(".git");
            let moved = workspace.with_file_name("original-git");
            std::fs::rename(&original, &moved).unwrap();
            let copy = std::process::Command::new("/bin/cp")
                .arg("-R")
                .arg(&moved)
                .arg(&original)
                .env_clear()
                .bounded_output_for(Duration::from_secs(10), 4096)
                .unwrap();
            assert!(
                copy.status.success(),
                "copy identical Git directory: {copy:?}"
            );
            assert_eq!(tree_snapshot(&original), tree_snapshot(&moved));
            release.take().unwrap().send(()).unwrap();
        }
    }
    let result = running.await;
    assert!(
        !unit.exists(),
        "{stage:?}: unit removed after confirmed cleanup"
    );
    drop(release);
    result
}

async fn record(state: &Path, workspace: &Path, call: &ToolCall) -> Recorded {
    let sessions = arany::list_sessions(StateRoot::open_existing(state).unwrap(), workspace.into())
        .await
        .unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let mut found = None;
    for session in sessions {
        let events = store.load_session(session.id).await.unwrap();
        let view = SessionView::replay(session.id, &events).unwrap().unwrap();
        if view
            .runs
            .iter()
            .any(|run| run.tools.iter().any(|tool| &tool.intent.call == call))
        {
            assert!(found.is_none(), "one Session owns this call");
            found = Some(Recorded { events, view });
        }
    }
    store.close().await.unwrap();
    let recorded = found.expect("journaled Git write");
    let tools: Vec<_> = recorded
        .view
        .runs
        .iter()
        .flat_map(|run| &run.tools)
        .filter(|tool| &tool.intent.call == call)
        .collect();
    assert_eq!(tools.len(), 1);
    let intent = tools[0].intent.id;
    let starts = recorded.events.iter().filter(|event| matches!(&event.event, Event::ToolStarted { intent: started, .. } if started.id == intent)).count();
    let finishes = recorded.events.iter().filter(|event| matches!(&event.event, Event::ToolFinished { observation, .. } if observation.intent.id == intent)).count();
    assert_eq!(
        (starts, finishes),
        (1, 1),
        "one start, one terminal observation; never retried"
    );
    recorded
}

fn observation(recorded: &Recorded) -> &arany::ToolObservation {
    recorded.view.runs.last().unwrap().tools[0]
        .observation
        .as_ref()
        .unwrap()
}

async fn capacity_released(engine: &Engine, workspace: &Path, name: &str, leftovers: &[String]) {
    assert_eq!(
        reservations(),
        0,
        "both reservations released after cleanup"
    );
    assert_eq!(
        unit_leftovers(),
        leftovers,
        "roots and volumes removed before capacity reuse"
    );
    let (outcome, _) = approved_git_write(engine, workspace, branch_creation(name)).await;
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        outcome.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        ToolDisposition::Succeeded
    );
    assert_eq!(unit_leftovers(), leftovers, "both capacity slots released");
}

pub(super) async fn run(
    engine: &Engine,
    root: &Path,
    workspace: &Path,
    state: &Path,
) -> Vec<Recorded> {
    // With exactly two slots, every subsequent write proves that both reservations were released.
    private_file(
        &StateRoot::account_path().unwrap().join("guard/limits.json"),
        br#"{"version":1,"units":2,"queued":32}"#,
    );
    let leftovers = unit_leftovers();
    let mut recorded = Vec::new();
    cancelled_rows(engine, workspace, state, &leftovers, &mut recorded).await;
    completed_rows(engine, workspace, state, &leftovers, &mut recorded).await;
    storage_rows(engine, root, workspace, state, &mut recorded).await;
    recorded.push(dead_publisher(engine, workspace, state).await);
    capacity_released(engine, workspace, "after-death", &leftovers).await;
    recorded
}

async fn cancelled_rows(
    engine: &Engine,
    workspace: &Path,
    state: &Path,
    leftovers: &[String],
    recorded: &mut Vec<Recorded>,
) {
    for stage in [
        GitWriteStage::BeforeGo,
        GitWriteStage::PartialGo,
        GitWriteStage::PublisherHeld,
    ] {
        let before = tree_snapshot(workspace);
        let (result, calls) = tokio::time::timeout(
            Duration::from_secs(120),
            gated(engine, workspace, stage, Action::Cancel),
        )
        .await
        .expect("cancelled publisher cleanup deadline");
        if stage != GitWriteStage::BeforeGo {
            assert!(
                matches!(result, Err(EngineError::Tool(arany::ToolError::Uncertain))),
                "{stage:?}: {result:?}"
            );
        }
        assert_eq!(calls, 1, "{stage:?}: no continuation");
        assert_eq!(
            tree_snapshot(workspace),
            before,
            "{stage:?}: held before first effect"
        );
        let row = record(
            state,
            workspace,
            &branch_creation(&format!("recovery-{stage:?}-Cancel")),
        )
        .await;
        let observed = observation(&row);
        assert_eq!(
            observed.disposition,
            if stage == GitWriteStage::BeforeGo {
                ToolDisposition::Cancelled
            } else {
                ToolDisposition::Uncertain
            }
        );
        assert!(observed.attestation.is_none());
        recorded.push(row);
        capacity_released(engine, workspace, &format!("after-{stage:?}"), leftovers).await;
    }
}

async fn completed_rows(
    engine: &Engine,
    workspace: &Path,
    state: &Path,
    leftovers: &[String],
    recorded: &mut Vec<Recorded>,
) {
    for (stage, action, expected) in [
        (
            GitWriteStage::FramePending,
            Action::Release,
            ToolDisposition::Succeeded,
        ),
        (
            GitWriteStage::BeforeGo,
            Action::Replace,
            ToolDisposition::Failed,
        ),
        (
            GitWriteStage::ExecutorTimeout,
            Action::Release,
            ToolDisposition::Limit,
        ),
    ] {
        let name = format!("recovery-{stage:?}-{action:?}");
        let call = branch_creation(&name);
        let before = tree_snapshot(workspace);
        let (result, calls) = tokio::time::timeout(
            Duration::from_secs(120),
            gated(engine, workspace, stage, action),
        )
        .await
        .expect("Git recovery gate deadline");
        assert!(result.is_ok(), "{stage:?}: {result:?}");
        assert_eq!(calls, 2, "{stage:?}: known outcomes permit continuation");
        let row = record(state, workspace, &call).await;
        assert_eq!(
            observation(&row).disposition,
            expected,
            "{stage:?}: {:?}",
            observation(&row)
        );
        if stage == GitWriteStage::ExecutorTimeout {
            assert_eq!(
                observation(&row).limit_cause,
                Some(arany::LimitCause::Timeout)
            );
            assert!(observation(&row).attestation.is_none());
        }
        if expected != ToolDisposition::Succeeded {
            assert_eq!(
                tree_snapshot(workspace),
                before,
                "{stage:?}: no publication"
            );
        }
        recorded.push(row);
        capacity_released(engine, workspace, &format!("reused-{stage:?}"), leftovers).await;
    }
}

async fn storage_rows(
    engine: &Engine,
    root: &Path,
    workspace: &Path,
    state: &Path,
    recorded: &mut Vec<Recorded>,
) {
    for fault in [
        arany::GitStorageFault::Refuse,
        arany::GitStorageFault::Exhaust,
    ] {
        let call = branch_creation(&format!("storage-{fault:?}"));
        let before = tree_snapshot(workspace);
        let leftovers = unit_leftovers();
        let evidence = root.join(format!("write-storage-{fault:?}.json"));
        arany::FaultPlan::fail_git_storage(call.clone(), fault, evidence.clone());
        let (outcome, _) = approved_git_write(engine, workspace, call.clone()).await;
        assert_eq!(
            outcome.run.tools[0]
                .observation
                .as_ref()
                .unwrap()
                .disposition,
            ToolDisposition::Failed
        );
        assert_eq!(
            tree_snapshot(workspace),
            before,
            "{fault:?}: Workspace and repository unchanged"
        );
        let row = record(state, workspace, &call).await;
        let (intent, unit): (uuid::Uuid, std::path::PathBuf) =
            serde_json::from_slice(&std::fs::read(evidence).unwrap()).unwrap();
        assert_eq!(intent, observation(&row).intent.id);
        assert!(!unit.exists());
        if fault == arany::GitStorageFault::Refuse {
            assert!(observation(&row).attestation.is_none());
        } else {
            assert!(
                observation(&row).attestation.is_some(),
                "Git reached its confined view after ENOSPC and the control-root canary check"
            );
        }
        recorded.push(row);
        capacity_released(
            engine,
            workspace,
            &format!("after-storage-{fault:?}"),
            &leftovers,
        )
        .await;
    }
}

async fn dead_publisher(engine: &Engine, workspace: &Path, state: &Path) -> Recorded {
    let call = branch_creation("publisher-died");
    arany::FaultPlan::kill_git_publisher_after_effect(call.clone());
    let (result, calls) = tokio::time::timeout(
        Duration::from_secs(120),
        drive(engine, workspace, call.clone(), RunCancellation::new()),
    )
    .await
    .unwrap();
    assert!(
        matches!(result, Err(EngineError::Tool(arany::ToolError::Uncertain))),
        "{result:?}"
    );
    assert_eq!(calls, 1);
    assert!(
        workspace.join(".git/refs/heads/publisher-died").is_file(),
        "publisher died after its first real effect"
    );
    let recorded = record(state, workspace, &call).await;
    let observed = observation(&recorded);
    assert_eq!(observed.disposition, ToolDisposition::Uncertain);
    assert!(observed.attestation.is_none());
    assert!(
        observed.output.contains(".git/HEAD.lock"),
        "controller reports recorded locks first: {}",
        observed.output
    );
    let before = tree_snapshot(workspace);
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut resumed = input(workspace);
    resumed.session_id = Some(recorded.view.id);
    resumed.title = None;
    resumed.objective.clear();
    resumed.goal = arany::GoalRequest::resume(recorded.view.goals.last().unwrap().id, false);
    let result = run_with(
        engine,
        &provider,
        resumed,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(
            result,
            Err(EngineError::Tool(arany::ToolError::StaleGitLocks(_)))
        ),
        "Goal resume must report the stale locks: {result:?}"
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        tree_snapshot(workspace),
        before,
        "resume neither deletes locks nor retries publication"
    );
    // The fixture acts as the human only after proving both reporting paths leave the locks alone.
    let record = workspace
        .join(".git")
        .join(format!("arany-locks-{}.json", observed.intent.id));
    let locks: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    for lock in locks {
        let path = workspace.join(".git").join(lock["path"].as_str().unwrap());
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    std::fs::remove_file(record).unwrap();
    recorded
}

pub(super) async fn replay(state: &Path, home: &Path, workspace: &Path, recorded: Vec<Recorded>) {
    let before = tree_snapshot(workspace);
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    for recorded in recorded {
        let id: SessionId = recorded.view.id;
        let events = store.load_session(id).await.unwrap();
        assert_eq!(
            events, recorded.events,
            "strict replay never appends or retries"
        );
        let view = SessionView::replay(id, &events).unwrap().unwrap();
        assert_eq!(view, recorded.view);
        crate::assert_closed_git_views(state, home, workspace, &events, &view);
    }
    store.close().await.unwrap();
    assert_eq!(tree_snapshot(workspace), before);
}

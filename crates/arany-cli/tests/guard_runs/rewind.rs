use super::*;
use arany::rewind::{CheckpointTarget, RewindOutcome};
use arany::test_support::goals::{facts_workspace, finish_with, replayed};
use arany::{CheckpointStatus, RewindIssue, RewindRequest};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn grant(state: &Path, write: bool) {
    let config = state.join("tools.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "version":1,"workspace_paths":["notes"],"write":write,"commands":[],"skills":[],"mcp":[]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
}

async fn case(case: &str) {
    arany::test_support::isolate_account_root();
    let temp = if case == "case_alias" {
        tempfile::Builder::new()
            .prefix("arany-rewind-apfs-")
            .tempdir_in("/private/tmp")
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    if case == "case_alias" {
        #[cfg(target_os = "macos")]
        assert_eq!(
            nix::sys::statfs::statfs(temp.path())
                .unwrap()
                .filesystem_type_name(),
            "apfs",
            "native rewind alias fixture requires APFS"
        );
        let lower = temp.path().join("a");
        let upper = temp.path().join("A");
        std::fs::write(&lower, b"synthetic APFS alias baseline").unwrap();
        let metadata = std::fs::metadata(&lower).unwrap();
        let alias = std::fs::metadata(&upper)
            .expect("native rewind alias fixture requires case-insensitive APFS");
        assert_eq!((metadata.dev(), metadata.ino()), (alias.dev(), alias.ino()));
        assert_eq!(metadata.nlink(), 1, "case alias is not a hard link");
        assert_eq!(
            std::fs::read(&upper).unwrap(),
            b"synthetic APFS alias baseline"
        );
    }
    let workspace = facts_workspace(temp.path(), "original");
    let state = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    grant(&state, true);
    let mut plan = write_plan();
    plan.paths = vec!["notes/facts.txt".into(), "notes/new.txt".into()];
    let partial = case == "partial";
    if partial {
        std::fs::create_dir(workspace.join("notes/sub")).unwrap();
        plan.paths.push("notes/sub/z.txt".into());
    }
    plan.budget = arany::GoalBudget::default();
    plan.success_criterion = arany::GoalSuccessCriterion::WorkspaceFileContent {
        path: "notes/facts.txt".into(),
        content: "changed".into(),
    };
    let mut script = vec![
        tool_outcome(
            arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: Some(hash(b"original")),
                content: "changed".into(),
            },
            Some(Box::new(plan)),
        ),
        tool_outcome(
            arany::ToolCall::Write {
                path: "notes/new.txt".into(),
                expected_digest: None,
                content: "new".into(),
            },
            None,
        ),
        finish_with("changed"),
    ];
    if partial {
        script.insert(
            2,
            tool_outcome(
                arany::ToolCall::Write {
                    path: "notes/sub/z.txt".into(),
                    expected_digest: None,
                    content: "z".into(),
                },
                None,
            ),
        );
    }
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut input = request(&workspace, "Change two files");
    input.goal.finish_after_run = false;
    let result = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let session = result.session_id;
    let (before_events, view) = replayed(&state, session).await;
    let target = view.rewinds.checkpoints[0].clone();
    let request = |target, kept: Vec<String>, overwrite: Vec<String>| RewindRequest {
        session_id: session,
        workspace: workspace.clone(),
        target,
        tools: ToolGrant::Configured,
        kept,
        overwrite,
    };
    let path = workspace.join("notes/facts.txt");
    let alias_identity = if case == "case_alias" {
        assert_eq!(
            result.run.tools[0].checkpoint.as_ref().unwrap().status,
            CheckpointStatus::Captured,
            "valid publication captures its before bytes before alias drift"
        );
        let before = std::fs::metadata(&path).unwrap();
        std::fs::rename(&path, workspace.join("notes/FACTS.txt")).unwrap();
        let alias = std::fs::metadata(workspace.join("notes/FACTS.txt")).unwrap();
        let lower = std::fs::metadata(&path).unwrap();
        let identity = (before.dev(), before.ino());
        assert_eq!(identity, (alias.dev(), alias.ino()));
        assert_eq!(identity, (lower.dev(), lower.ino()));
        assert_eq!(alias.nlink(), 1);
        let mut names = std::fs::read_dir(workspace.join("notes"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["FACTS.txt", "new.txt"]);
        Some(identity)
    } else {
        None
    };
    if ["drift", "keep", "overwrite"].contains(&case) {
        std::fs::write(&path, "external").unwrap();
    }
    if case == "link" {
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("new.txt", &path).unwrap();
    }
    if case == "special" {
        std::fs::remove_file(&path).unwrap();
        std::os::unix::net::UnixListener::bind(&path).unwrap();
    }
    if case == "grant" {
        grant(&state, false);
    }
    if case == "missing" {
        let db = rusqlite::Connection::open(state.join("checkpoints.sqlite")).unwrap();
        db.execute("DELETE FROM refs", []).unwrap();
        db.execute("DELETE FROM blobs", []).unwrap();
    }
    if case == "hooks" {
        let path = state.join("permissions.json");
        std::fs::write(&path, br#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"exit 2","timeout":5}]}],"PostToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"exit 2","timeout":5}]}]}}"#).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let kept = if case == "keep" {
        vec!["notes/facts.txt".into()]
    } else {
        Vec::new()
    };
    let overwrite = if case == "overwrite" {
        vec!["notes/facts.txt".into()]
    } else {
        Vec::new()
    };
    let preview = engine
        .rewind_preview(request(target.clone(), kept, overwrite))
        .await
        .unwrap();
    if ["drift", "link", "special", "grant", "missing", "case_alias"].contains(&case) {
        assert!(!preview.can_restore(), "{case}");
        let expected = match case {
            "drift" => RewindIssue::Drifted,
            "grant" => RewindIssue::OutsideGrant,
            "missing" => RewindIssue::Unavailable,
            _ => RewindIssue::Unsafe,
        };
        assert!(preview.files()[0].issues.contains(&expected), "{case}");
        assert!(matches!(
            engine.rewind(preview, RunCancellation::new()).await,
            Err(EngineError::RewindBlocked)
        ));
        assert!(
            workspace.join("notes/new.txt").exists(),
            "whole batch refused"
        );
        engine.close().await.unwrap();
        let (after_events, after) = replayed(&state, session).await;
        assert!(after.rewinds.operations.is_empty());
        if let Some(identity) = alias_identity {
            assert_eq!(
                after_events, before_events,
                "alias refusal appends no Events"
            );
            assert_eq!(after, view, "closed replay preserves the Session");
            for name in ["facts.txt", "FACTS.txt"] {
                let alias = workspace.join("notes").join(name);
                let metadata = std::fs::metadata(&alias).unwrap();
                assert_eq!((metadata.dev(), metadata.ino()), identity);
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(std::fs::read(alias).unwrap(), b"changed");
            }
            assert_eq!(
                std::fs::read(workspace.join("notes/new.txt")).unwrap(),
                b"new"
            );
        }
        return;
    }
    assert!(preview.can_restore());
    let rewind_id = preview.rewind_id();
    if case == "receipt_loss" {
        arany::FaultPlan::lose_guard_receipt(rewind_id);
    }
    if case == "finish_loss" {
        arany::FaultPlan::fail_goal_append(session, |event| {
            matches!(event, Event::WorkspaceRewindFinished { .. })
        });
    }
    if case == "race" {
        std::fs::write(&path, "raced").unwrap();
    }
    let cancellation = RunCancellation::new();
    if case == "cancel" {
        cancellation.cancel();
    }
    let gate = ["go_race", "cancel_started", "crash_captured"]
        .contains(&case)
        .then(|| arany::FaultPlan::gate_checkpoint(session));
    let ack = (case == "crash_started").then(|| {
        arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
            matches!(event, Event::WorkspaceRewindStarted { .. })
        })
    });
    if partial {
        std::fs::set_permissions(
            workspace.join("notes/sub"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
    }
    let result = 'run: {
        let restore = engine.rewind(preview, cancellation.clone());
        tokio::pin!(restore);
        if let Some((entered, release)) = gate {
            tokio::select! {
                result = &mut restore => panic!("rewind completed before capture gate: {result:?}"),
                entered = tokio::time::timeout(Duration::from_secs(120), entered) => entered.unwrap().unwrap(),
            }
            match case {
                "go_race" => std::fs::write(&path, "raced").unwrap(),
                "crash_captured" => {
                    drop(release);
                    break 'run None;
                }
                _ => {
                    cancellation.cancel();
                }
            }
            let _ = release.send(());
        }
        if let Some((entered, release)) = ack {
            tokio::select! {
                result = &mut restore => panic!("rewind completed before the Started acknowledgment: {result:?}"),
                entered = tokio::time::timeout(Duration::from_secs(120), entered) => entered.unwrap().unwrap(),
            }
            drop(release);
            assert!(restore.await.is_err());
            break 'run None;
        }
        Some(restore.await)
    };
    let Some(result) = result else {
        let again = request(target.clone(), Vec::new(), Vec::new());
        crash(engine, &workspace, &state, session, &view, &path, again).await;
        return;
    };
    if partial {
        std::fs::set_permissions(
            workspace.join("notes/sub"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    if case == "race" {
        assert!(
            matches!(result, Err(EngineError::RewindPreviewExpired)),
            "{result:?}"
        );
        assert!(workspace.join("notes/new.txt").exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"raced");
    } else if case == "cancel" {
        assert!(
            matches!(result, Err(EngineError::CancelledBeforeStart)),
            "{result:?}"
        );
        assert!(workspace.join("notes/new.txt").exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed");
    } else if ["go_race", "cancel_started"].contains(&case) {
        let finished = result.unwrap();
        assert_eq!(
            finished.outcome,
            if case == "go_race" {
                RewindOutcome::Conflict
            } else {
                RewindOutcome::Cancelled
            }
        );
        assert!(finished.attestation.is_none());
        if case == "go_race" {
            assert_eq!(finished.conflicts, ["notes/facts.txt"]);
            assert_eq!(finished.conflict_count, 1);
        }
        assert!(
            workspace.join("notes/new.txt").exists(),
            "no partial publication"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if case == "go_race" {
                b"raced".as_slice()
            } else {
                b"changed".as_slice()
            }
        );
    } else {
        if case == "finish_loss" {
            assert!(result.is_err());
        } else {
            let finished = result.unwrap();
            assert_eq!(
                finished.outcome,
                if ["receipt_loss", "partial"].contains(&case) {
                    RewindOutcome::Uncertain
                } else {
                    RewindOutcome::Restored
                }
            );
            if !["receipt_loss", "partial"].contains(&case) {
                assert_eq!(
                    finished.checkpoint.as_ref().unwrap().status,
                    CheckpointStatus::Captured
                );
            }
        }
        assert!(!workspace.join("notes/new.txt").exists());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if case == "keep" {
                b"external".as_slice()
            } else {
                b"original".as_slice()
            }
        );
        if ["receipt_loss", "finish_loss", "partial"].contains(&case) {
            blocked(
                &engine,
                &workspace,
                session,
                &view,
                request(target.clone(), Vec::new(), Vec::new()),
            )
            .await;
        } else if case != "keep" {
            let (_, restored) = replayed(&state, session).await;
            let boundary = restored.rewinds.checkpoints.iter().find(|c| matches!(c, CheckpointTarget::Rewind { rewind_id: id, .. } if *id == rewind_id)).unwrap().clone();
            let undo = engine
                .rewind_preview(request(boundary, Vec::new(), Vec::new()))
                .await
                .unwrap();
            assert!(undo.can_restore(), "Before rewind is available");
            assert_eq!(
                engine
                    .rewind(undo, RunCancellation::new())
                    .await
                    .unwrap()
                    .outcome,
                RewindOutcome::Restored
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                if case == "overwrite" {
                    b"external".as_slice()
                } else {
                    b"changed".as_slice()
                }
            );
            assert_eq!(
                std::fs::read(workspace.join("notes/new.txt")).unwrap(),
                b"new"
            );
        }
    }
    settle(engine, &state, session, &path, case).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "native case-insensitive APFS rewind alias boundary; explicit opt-in"]
async fn guarded_rewind_refuses_case_insensitive_apfs_alias_without_publication() {
    case("case_alias").await;
}

async fn crash(
    engine: Engine,
    workspace: &Path,
    state: &Path,
    session: SessionId,
    view: &SessionView,
    path: &Path,
    again: arany::RewindRequest,
) {
    assert!(workspace.join("notes/new.txt").exists());
    assert_eq!(std::fs::read(path).unwrap(), b"changed");
    blocked(&engine, workspace, session, view, again).await;
    settle(engine, state, session, path, "crash").await;
}

async fn blocked(
    engine: &Engine,
    workspace: &Path,
    session: SessionId,
    view: &SessionView,
    again: arany::RewindRequest,
) {
    let blocked = engine.rewind_preview(again).await.unwrap();
    assert!(!blocked.can_restore());
    assert!(!blocked.plan().blockers.is_empty());
    let mut resume = request_for_resume(workspace, session, result_goal(view));
    resume.include_paths.clear();
    let never = Arc::new(ScriptedProvider::new(Vec::new()));
    assert!(
        run_with(
            engine,
            &never,
            resume,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(never.requests.lock().unwrap().is_empty());
}

async fn settle(engine: Engine, state: &Path, session: SessionId, path: &Path, case: &str) {
    engine.close().await.unwrap();
    let before = std::fs::read(path).unwrap();
    let (events, first) = replayed(state, session).await;
    let reopened = engine_support::open(
        StateRoot::open_existing(state).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    reopened.close().await.unwrap();
    let (again, second) = replayed(state, session).await;
    assert_eq!(events, again, "reopen fabricates no Events");
    let finished = events.iter().rev().find_map(|e| match &e.event {
        Event::WorkspaceRewindFinished { record } => Some(record),
        _ => None,
    });
    match case {
        "receipt_loss" | "partial" => {
            let finished = finished.expect("Finished is persisted");
            assert_eq!(finished.outcome, RewindOutcome::Uncertain);
            let reported = case == "partial";
            assert_eq!(finished.attestation.is_some(), reported, "{case}");
            assert_eq!(finished.checkpoint.is_some(), reported, "{case}");
        }
        "crash" | "finish_loss" => {
            assert!(finished.is_none());
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e.event, Event::WorkspaceRewindStarted { .. }))
            );
        }
        _ => {}
    }
    assert_eq!(first, second);
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "strict replay never republishes"
    );
}

fn result_goal(view: &SessionView) -> arany::GoalId {
    view.goals[0].id
}
fn request_for_resume(
    workspace: &Path,
    session: SessionId,
    goal: arany::GoalId,
) -> arany::RunRequest {
    let mut input = request(workspace, "");
    input.session_id = Some(session);
    input.goal = arany::GoalRequest::resume(goal, true);
    input
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn guarded_rewind_success_drift_choices_and_undo() {
    for name in ["success", "drift", "keep", "overwrite", "hooks"] {
        case(name).await;
    }
}
#[tokio::test]
async fn guarded_rewind_refuses_missing_grant_links_special_files_and_stale_confirmation() {
    for name in [
        "missing",
        "grant",
        "link",
        "special",
        "race",
        "cancel",
        "go_race",
        "cancel_started",
    ] {
        case(name).await;
    }
}
#[tokio::test]
async fn guarded_rewind_lost_receipt_and_crash_prefix_block_resume_without_reexecution() {
    for name in [
        "receipt_loss",
        "finish_loss",
        "partial",
        "crash_started",
        "crash_captured",
    ] {
        case(name).await;
    }
}

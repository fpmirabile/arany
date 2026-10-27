use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const TARGET_BYTES: &[u8] = b"target\0\xff\n";

fn call(branch: &str) -> ToolCall {
    ToolCall::GitSwitch {
        branch: branch.into(),
    }
}

fn prepare(workspace: &Path, home: &Path) {
    for (path, bytes) in [
        ("src/removed.txt", "removed\n"),
        ("src/keep.txt", "base\n"),
        ("src/run.txt", "plain\n"),
    ] {
        std::fs::write(workspace.join(path), bytes).unwrap();
    }
    git_text(workspace, home, &["add", "src"]);
    host_git(CLT_GIT, workspace, home, &["commit", "-qm", "switch base"]);
    git_text(workspace, home, &["checkout", "-qb", "topic"]);
    std::fs::write(workspace.join("src/a.txt"), TARGET_BYTES).unwrap();
    std::fs::remove_file(workspace.join("src/removed.txt")).unwrap();
    std::fs::create_dir_all(workspace.join("src/nested")).unwrap();
    std::fs::write(workspace.join("src/nested/new.sh"), "new executable\n").unwrap();
    for path in ["src/run.txt", "src/nested/new.sh"] {
        std::fs::set_permissions(workspace.join(path), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    git_text(workspace, home, &["add", "-A"]);
    host_git(
        CLT_GIT,
        workspace,
        home,
        &["commit", "-qm", "switch target"],
    );
    git_text(workspace, home, &["checkout", "-q", "main"]);
    for branch in [
        "conflict-worktree",
        "conflict-index",
        "conflict-untracked",
        "held-target",
        "racing-content",
        "racing-mode",
        "racing-creation",
        "publisher-died",
    ] {
        git_text(workspace, home, &["branch", branch, "topic"]);
    }
    std::fs::write(workspace.join("src/keep.txt"), "staged\n").unwrap();
    git_text(workspace, home, &["add", "src/keep.txt"]);
    std::fs::write(workspace.join("src/keep.txt"), "worktree\n").unwrap();
}

async fn successful_switches(
    engine: &arany::Engine,
    workspace: &Path,
    home: &Path,
) -> Vec<arany::RunOutcome> {
    let before = std::fs::metadata(workspace.join("src/a.txt"))
        .unwrap()
        .ino();
    let index = std::fs::read(workspace.join(".git/index")).unwrap();
    let (switched, result) = succeeded_write(engine, workspace, call("topic")).await;
    assert_switched(workspace, home, &result, &index, before);
    let added = workspace.join("src/nested/new.sh");
    let before = tree_snapshot(workspace);
    let (same, result) = succeeded_write(engine, workspace, call("topic")).await;
    assert_eq!(
        result,
        json!({"branch": "topic", "switched": false, "publication": {"files": 0, "objects": 0}})
    );
    assert_eq!(
        tree_snapshot(workspace),
        before,
        "same branch publishes nothing"
    );
    let (returned, _) = succeeded_write(engine, workspace, call("main")).await;
    assert_eq!(
        std::fs::read(workspace.join("src/a.txt")).unwrap(),
        b"alpha\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/removed.txt")).unwrap(),
        b"removed\n"
    );
    assert!(!added.exists());
    assert_eq!(
        git_text(workspace, home, &["diff", "--cached", "--name-only"]),
        "src/keep.txt\n"
    );
    vec![switched, same, returned]
}

fn assert_switched(
    workspace: &Path,
    home: &Path,
    result: &serde_json::Value,
    index: &[u8],
    before: u64,
) {
    assert_eq!(result["switched"], true);
    assert_eq!(result["files"], 4);
    assert_eq!(result["publication"]["objects"], 0);
    assert_eq!(result["publication"]["files"], 4);
    assert_eq!(
        result["publication"]["head"],
        json!({"before": "ref: refs/heads/main", "after": "ref: refs/heads/topic"})
    );
    assert_eq!(result["publication"]["index"]["before"], hash(index));
    assert_eq!(
        result["publication"]["index"]["after"],
        hash(&std::fs::read(workspace.join(".git/index")).unwrap())
    );
    assert_eq!(
        std::fs::read(workspace.join("src/a.txt")).unwrap(),
        TARGET_BYTES
    );
    assert_ne!(
        std::fs::metadata(workspace.join("src/a.txt"))
            .unwrap()
            .ino(),
        before
    );
    assert!(!workspace.join("src/removed.txt").exists());
    let added = workspace.join("src/nested/new.sh");
    assert_eq!(std::fs::read(&added).unwrap(), b"new executable\n");
    assert_eq!(
        std::fs::metadata(&added).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_ne!(
        std::fs::metadata(workspace.join("src/run.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        git_text(workspace, home, &["diff", "--cached", "--name-only"]),
        "src/keep.txt\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/keep.txt")).unwrap(),
        b"worktree\n"
    );
}

async fn conflicts(
    engine: &arany::Engine,
    workspace: &Path,
    home: &Path,
    state: &Path,
) -> Vec<recovery::Recorded> {
    let mut rows = Vec::new();
    for branch in [
        "conflict-worktree",
        "conflict-index",
        "conflict-untracked",
        "held-target",
    ] {
        let lock = workspace.join(".git/refs/heads/held-target.lock");
        if branch == "conflict-untracked" {
            std::fs::write(workspace.join("src/nested/new.sh"), "untracked\n").unwrap();
        } else if branch == "held-target" {
            std::fs::write(&lock, "held\n").unwrap();
        } else {
            std::fs::write(workspace.join("src/a.txt"), "local change\n").unwrap();
            if branch == "conflict-index" {
                git_text(workspace, home, &["add", "src/a.txt"]);
            }
        }
        let before = tree_snapshot(workspace);
        let call = call(branch);
        let (outcome, _) = approved_git_write(engine, workspace, call.clone()).await;
        let observed = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observed.disposition,
            ToolDisposition::Failed,
            "{branch}: {observed:?}"
        );
        assert!(!observed.output.contains("publication"));
        assert!(
            observed.output.contains(if branch == "held-target" {
                "nothing was published"
            } else {
                "overwrite local changes or untracked files"
            }),
            "{observed:?}"
        );
        assert_eq!(
            tree_snapshot(workspace),
            before,
            "{branch}: nothing published"
        );
        rows.push(recovery::record(state, workspace, &call).await);
        if branch == "held-target" {
            std::fs::remove_file(lock).unwrap();
        } else if branch == "conflict-untracked" {
            std::fs::remove_file(workspace.join("src/nested/new.sh")).unwrap();
        } else {
            git_text(workspace, home, &["reset", "-q", "HEAD", "--", "src/a.txt"]);
            std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
        }
    }
    rows
}

#[tokio::test]
#[ignore = "isolated helper owned by native Git write journey"]
async fn native_git_switch_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let WriteFixture {
        home,
        workspace,
        state_path,
        engine,
        ..
    } = write_fixture(Path::new(&root), "git-switch");
    let leftovers = unit_leftovers();
    prepare(&workspace, &home);
    let outcomes = successful_switches(&engine, &workspace, &home).await;
    crate::assert_no_new_units(&leftovers, "successful switches leave no unit");
    let mut rows = conflicts(&engine, &workspace, &home, &state_path).await;
    rows.extend(recovery::switch_drift(&engine, &workspace, &state_path).await);
    rows.push(recovery::switch_death(&engine, &workspace, &state_path).await);
    engine.close().await.unwrap();
    crate::assert_no_new_units(&leftovers, "all units and volumes cleaned up");
    std::fs::rename(workspace.join(".git"), home.join("repository-after-run")).unwrap();
    recovery::replay(&state_path, &home, &workspace, rows).await;
    let before = tree_snapshot(&workspace);
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    for outcome in outcomes {
        let events = store.load_session(outcome.session_id).await.unwrap();
        let view = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(view.runs.last().unwrap(), &outcome.run);
        let started = events.iter().filter(|event| matches!(&event.event, arany::Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::GitSwitch { .. }))).count();
        let finished = events.iter().filter(|event| matches!(&event.event, arany::Event::ToolFinished { observation, .. } if matches!(observation.intent.call, ToolCall::GitSwitch { .. }))).count();
        assert_eq!(
            (started, finished),
            (1, 1),
            "one start and one terminal observation; replay never re-executes a switch"
        );
        crate::assert_closed_git_views(&state_path, &home, &workspace, &events, &view);
    }
    store.close().await.unwrap();
    assert_eq!(tree_snapshot(&workspace), before);
}

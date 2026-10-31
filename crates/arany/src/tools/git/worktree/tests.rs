use super::super::tests::{environment, fixture, git_program, host_git_output};
use crate::session::{AgentRunId, SessionId};

#[test]
fn editor_worktrees_check_out_copy_untracked_integrate_and_clean_up() {
    crate::test_support::isolate_account_root();
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    std::fs::write(workspace.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(workspace.join("ignored.txt"), "ignored\n").unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut runtime = crate::tools::tests::in_process_runtime(
        &workspace,
        serde_json::json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.cache_state = Some(std::sync::Arc::new(state.try_clone().unwrap()));
    runtime.config.git = Some(git_program());
    runtime.git_environment = Some(std::sync::Arc::new(environment(
        &workspace,
        &state.path().join("editors"),
        &home,
    )));
    let (session, id) = (SessionId::new(), AgentRunId::new());
    runtime.create_editor_workspace(session, id, None).unwrap();
    let tree = runtime.editor_root(id).unwrap().path;
    assert_eq!(
        tree.file_name().unwrap().to_str().unwrap(),
        format!("arany-{id}")
    );
    assert!(
        std::fs::symlink_metadata(tree.join(".git"))
            .unwrap()
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("src/a.txt")).unwrap(),
        "delta\n"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("src/new.txt")).unwrap(),
        "new\n"
    );
    assert!(!tree.join(".env").exists() && !tree.join("ignored.txt").exists());

    std::fs::write(tree.join("src/a.txt"), "editor\n").unwrap();
    std::fs::write(tree.join("src/added.txt"), "added\n").unwrap();
    std::fs::remove_file(tree.join("src/new.txt")).unwrap();
    let changes = runtime.editor_changes(id, 64, 1 << 20).unwrap();
    let paths: Vec<_> = changes.iter().map(|change| change.path.as_str()).collect();
    assert_eq!(paths, ["src/a.txt", "src/added.txt", "src/new.txt"]);
    assert_eq!(
        changes[0].before.as_ref().unwrap().sha256,
        crate::validate::hex_digest(b"delta\n")
    );
    assert!(changes[1].before.is_none() && changes[2].after.is_none());

    runtime.release_editor_workspace(session, id, true);
    assert!(!tree.exists());
    let listed = host_git_output(&workspace, &home, &["worktree", "list", "--porcelain"]);
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("arany-"));
    assert!(
        !workspace
            .join(".git/worktrees")
            .join(format!("arany-{id}"))
            .exists()
    );
}

#[test]
fn a_folder_without_git_keeps_arany_s_own_copy() {
    crate::test_support::isolate_account_root();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "base").unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut runtime = crate::tools::tests::in_process_runtime(
        &workspace,
        serde_json::json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.cache_state = Some(std::sync::Arc::new(state.try_clone().unwrap()));
    runtime.config.git = Some(git_program());
    runtime.git_environment = Some(std::sync::Arc::new(environment(
        &workspace,
        &state.path().join("editors"),
        &temp.path().join("home"),
    )));
    let (session, id) = (SessionId::new(), AgentRunId::new());
    runtime.create_editor_workspace(session, id, None).unwrap();
    let tree = runtime.editor_root(id).unwrap().path;
    assert!(tree.ends_with(crate::tools::fs::editor::TREE));
    assert!(!tree.join(".git").exists());
}

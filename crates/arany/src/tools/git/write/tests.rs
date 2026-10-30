use super::super::tests::{environment, fixture, git_program, host_git, host_git_output, scratch};
use super::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn write(temp: &Path, workspace: &Path, call: &ToolCall) -> (Value, GitRefs, Option<i32>) {
    let program = git_program();
    let environment = environment(workspace, &scratch(temp, 900), &temp.join("home"));
    let git = Git::workspace(&program, &environment);
    let written = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(execute(
            &git,
            call,
            &HiddenPaths::default(),
            16 * 1024,
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        ))
        .expect("typed Git write");
    (
        serde_json::from_str(&written.output).unwrap(),
        *written.refs.expect("write refs"),
        written.exit_code,
    )
}

fn git_text(workspace: &Path, home: &Path, args: &[&str]) -> String {
    let output = host_git_output(workspace, home, args);
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn commit(paths: &[&str], staged: bool) -> ToolCall {
    ToolCall::GitCommit {
        message: "typed commit".into(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        staged,
    }
}

#[test]
fn commit_publishes_named_paths_without_protected_files_or_hooks() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    let marker = temp.path().join("hook-ran");
    let hook = workspace.join(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    host_git(&workspace, &home, &["config", "user.name", "Typed Writer"]);
    host_git(
        &workspace,
        &home,
        &["config", "user.email", "typed@example.invalid"],
    );

    let (value, refs, exit) = write(temp.path(), &workspace, &commit(&["."], false));
    assert_eq!(exit, Some(0));
    assert_eq!(value["branch"], "main");
    assert_eq!(refs.before.head, "refs/heads/main");
    assert_ne!(refs.before.oid, refs.after.oid);
    assert_eq!(value["commit"], json!(refs.after.oid));
    let files = git_text(
        &workspace,
        &home,
        &["show", "--name-only", "--format=", "HEAD"],
    );
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        ["src/a.txt", "src/big.txt", "src/new.txt"]
    );
    assert_eq!(
        git_text(&workspace, &home, &["log", "-1", "--format=%an"]).trim(),
        "Typed Writer"
    );
    assert!(!marker.exists(), "a declined hook ran");
}

#[test]
fn commit_from_a_detached_head_refuses_and_records_unchanged_refs() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    host_git(
        &workspace,
        &temp.path().join("home"),
        &["checkout", "-q", "--detach"],
    );
    let (value, refs, _) = write(temp.path(), &workspace, &commit(&["src/a.txt"], false));
    assert_eq!(value["failed"], DETACHED);
    assert_eq!(refs.before, refs.after);
    assert_eq!(refs.before.head, "detached");
}

#[test]
fn branch_creation_and_switch_use_native_git_refusals() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    let create = |name: &str, start: Option<&str>| ToolCall::GitBranch {
        name: Some(name.into()),
        start_point: start.map(str::to_owned),
        scope: None,
        base: None,
        max_count: None,
    };
    let (value, refs, _) = write(temp.path(), &workspace, &create("topic", None));
    assert_eq!(value["branch"], "topic");
    assert_eq!(value["oid"], json!(refs.after.oid));
    assert_eq!(refs.before.target.as_ref().unwrap().oid, None);
    let (value, _, exit) = write(temp.path(), &workspace, &create("topic", None));
    assert!(value["failed"].is_string() && exit != Some(0));

    let switch = |branch: &str| ToolCall::GitSwitch {
        branch: branch.into(),
    };
    let (value, refs, _) = write(temp.path(), &workspace, &switch("main"));
    assert_eq!(value, json!({"branch": "main", "switched": false}));
    assert_eq!(refs.before, refs.after);
    let (value, _, _) = write(temp.path(), &workspace, &switch("missing"));
    assert_eq!(value["failed"], "No local branch has that name.");
    let (value, refs, _) = write(temp.path(), &workspace, &switch("topic"));
    assert_eq!(value, json!({"branch": "topic", "switched": true}));
    assert_eq!(refs.after.head, "refs/heads/topic");

    // The dirty src/a.txt differs between the branches, so Git refuses to overwrite it.
    let (_, _, _) = write(temp.path(), &workspace, &create("older", Some("HEAD~1")));
    let (value, refs, exit) = write(temp.path(), &workspace, &switch("older"));
    assert!(value["failed"].is_string() && exit != Some(0), "{value}");
    assert_eq!(refs.after.head, "refs/heads/topic");
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/a.txt")).unwrap(),
        "delta\n"
    );
    host_git(&workspace, &home, &["checkout", "-q", "--detach"]);
    let (value, _, _) = write(temp.path(), &workspace, &switch("main"));
    assert_eq!(value["failed"], DETACHED_SWITCH);
}

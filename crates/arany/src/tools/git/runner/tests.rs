use super::super::tests::{environment, fixture, git_program, host_git, scratch};
use super::*;
use std::os::unix::fs::PermissionsExt;

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(30)
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn admit(temp: &Path, workspace: &Path) -> Option<String> {
    let program = git_program();
    let environment = environment(workspace, &scratch(temp, 700), &temp.join("home"));
    block_on(Git::workspace(&program, &environment).admit(deadline())).unwrap()
}

#[test]
fn executable_keys_cover_programs_and_shell_aliases_only() {
    for key in [
        "filter.lfs.process",
        "diff.evil.textconv",
        "diff.external",
        "merge.ours.driver",
        "core.sshcommand",
        "credential.helper",
        "credential.https://example.com.helper",
        "gpg.ssh.program",
        "pager.log",
        "remote.origin.uploadpack",
    ] {
        assert!(executable_key(key, "x"), "{key}");
    }
    assert!(executable_key("alias.run", "!scripts/run"));
    for (key, value) in [
        ("alias.st", "status"),
        ("core.filemode", "true"),
        ("remote.origin.url", "./repo"),
        ("filter.lfs.required", "true"),
    ] {
        assert!(!executable_key(key, value), "{key}");
    }
}

#[test]
fn configuration_that_includes_or_runs_a_folder_file_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    assert_eq!(admit(temp.path(), &workspace), None);

    std::fs::write(workspace.join("team.cfg"), "[core]\n\tfilemode = false\n").unwrap();
    host_git(
        &workspace,
        &home,
        &["config", "include.path", "../team.cfg"],
    );
    let refusal = admit(temp.path(), &workspace).unwrap();
    assert!(
        refusal.contains("team.cfg") && refusal.contains("did not run Git"),
        "{refusal}"
    );
    host_git(&workspace, &home, &["config", "--unset", "include.path"]);

    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
            workspace.display(),
            workspace.join("team.cfg").display()
        ),
    )
    .unwrap();
    assert!(admit(temp.path(), &workspace).is_some());
    std::fs::remove_file(home.join(".gitconfig")).unwrap();

    for (key, value) in [
        ("filter.evil.clean", "./scripts/clean.sh"),
        ("filter.evil.smudge", "sh scripts/smudge"),
        ("alias.run", "!scripts/run --now"),
        (
            "core.sshCommand",
            &format!("{}/bin/ssh", workspace.display()),
        ),
    ] {
        host_git(&workspace, &home, &["config", key, value]);
        let refusal = admit(temp.path(), &workspace).unwrap_or_default();
        assert!(refusal.contains("would run a program"), "{key}: {refusal}");
        host_git(&workspace, &home, &["config", "--unset", key]);
    }
    // Programs outside the folder, including in `.git`, are the user's to run.
    for (key, value) in [
        ("filter.lfs.clean", "git-lfs clean -- %f"),
        ("filter.copy.clean", "/usr/bin/cat"),
        ("credential.helper", "store --file ~/.git-credentials"),
        ("alias.st", "status"),
    ] {
        host_git(&workspace, &home, &["config", key, value]);
    }
    assert_eq!(admit(temp.path(), &workspace), None);
}

#[test]
fn the_cage_disables_the_users_fsmonitor_hooks_and_pager() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    let markers = temp.path().join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    let fsmonitor = temp.path().join("bin/fsmonitor");
    script(
        &fsmonitor,
        &format!("touch {}/fsmonitor", markers.display()),
    );
    let hook = temp.path().join("hooks/post-index-change");
    script(&hook, &format!("touch {}/hook", markers.display()));
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n\tpager = touch {}/pager\n",
            fsmonitor.display(),
            hook.parent().unwrap().display(),
            markers.display()
        ),
    )
    .unwrap();
    let program = git_program();
    let environment = environment(&workspace, &scratch(temp.path(), 701), &home);
    let git = Git::workspace(&program, &environment);
    let argv = ["status", "--porcelain"].map(String::from);
    let captured = block_on(git.run(&argv, Io::default(), deadline())).unwrap();
    assert_eq!(captured.status, Some(0));
    let argv = ["add", "-N", "src/new.txt"].map(String::from);
    assert_eq!(
        block_on(git.run(&argv, Io::default(), deadline()))
            .unwrap()
            .status,
        Some(0)
    );
    assert_eq!(std::fs::read_dir(&markers).unwrap().count(), 0);
}

#[test]
fn path_lookup_skips_folder_directories_and_the_pin_detects_change() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let planted = workspace.join("bin/git");
    script(&planted, "exit 0");
    let mut path = std::ffi::OsString::from(workspace.join("bin"));
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap());
    let environment = Environment::from_variables(&workspace, temp.path(), |name| match name {
        "HOME" => Some(temp.path().into()),
        "PATH" => Some(path.clone()),
        _ => None,
    })
    .unwrap();
    let program = resolve(&environment).unwrap();
    assert_eq!(program.sha256, git_program().sha256);
    assert!(!Path::new(&program.executable).starts_with(&workspace));
    assert!(verify(&program).is_ok());
    let changed = Program {
        sha256: "0".repeat(64),
        ..program
    };
    assert!(matches!(verify(&changed), Err(ToolError::ChangedInput)));
}

#[test]
fn deadline_stops_the_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    let home = temp.path().join("home");
    std::fs::write(home.join(".gitconfig"), "[alias]\n\tslow = !sleep 30\n").unwrap();
    let program = git_program();
    let environment = environment(&workspace, &scratch(temp.path(), 702), &home);
    let git = Git::workspace(&program, &environment);
    let started = std::time::Instant::now();
    let result = block_on(git.run(
        &["slow".to_owned()],
        Io::default(),
        tokio::time::Instant::now() + Duration::from_millis(300),
    ));
    assert!(matches!(result, Err(ToolError::Timeout(_))));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn stale_locks_are_listed_never_removed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = fixture(temp.path());
    for lock in [".git/index.lock", ".git/refs/heads/main.lock"] {
        std::fs::write(workspace.join(lock), "").unwrap();
    }
    std::fs::write(workspace.join(".git/objects/stray.lock"), "").unwrap();
    let mut locks = stale_locks(&workspace);
    locks.sort();
    assert_eq!(locks, [".git/index.lock", ".git/refs/heads/main.lock"]);
    assert!(workspace.join(".git/index.lock").exists());
}

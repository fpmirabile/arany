use super::super::tests::{
    RestoreMode, arm_hostile, assert_unchanged, fixture, host_git, host_git_output, run_call,
    scratch, tree,
};
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage {
    Snapshot,
    Checked,
    Planned,
    Locked,
}

type Race = (PathBuf, Stage, Box<dyn FnOnce() + Send>);

// Per-Workspace so parallel tests never trigger each other's races.
static RACES: std::sync::Mutex<Vec<Race>> = std::sync::Mutex::new(Vec::new());

pub(super) fn race(workspace: &Path, stage: Stage) {
    let race = {
        let mut races = RACES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        races
            .iter()
            .position(|(path, at, _)| path == workspace && *at == stage)
            .map(|index| races.remove(index).2)
    };
    if let Some(race) = race {
        race();
    }
}

fn arm(repo: &Repo, stage: Stage, args: &'static [&'static [&'static str]]) {
    let (workspace, home) = (repo.workspace.clone(), repo.home.clone());
    RACES.lock().unwrap().push((
        repo.workspace.clone(),
        stage,
        Box::new(move || {
            for args in args {
                host_git(&workspace, &home, args);
            }
        }),
    ));
}

fn identity() -> GitIdentity {
    GitIdentity {
        name: "Arany Writer".into(),
        email: "writer@example.invalid".into(),
    }
}

struct Repo {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    runs: usize,
    intent: uuid::Uuid,
}

impl Repo {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        Self {
            temp,
            workspace,
            home,
            runs: 0,
            intent: uuid::Uuid::nil(),
        }
    }

    fn write(&mut self, call: ToolCall) -> Result<Value, ToolError> {
        self.runs += 1;
        let git = super::super::discover().expect("pinned system git");
        let scratch = scratch(self.temp.path(), self.runs);
        self.intent = uuid::Uuid::now_v7();
        let intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: self.intent,
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: crate::tools::types::ToolLimits {
                result_bytes: 16 * 1024,
                ..Default::default()
            },
            expires_at_ms: 1,
            use_count: 1,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(execute(
                &self.workspace,
                &scratch,
                &git,
                &identity(),
                &intent,
                Duration::from_secs(30),
            ))
            .map(|output| serde_json::from_str(&output).unwrap())
    }

    fn succeeded(&mut self, call: ToolCall) -> Value {
        let value = self.write(call).expect("typed Git write");
        assert!(value.get("failed").is_none(), "{value}");
        value
    }

    fn git(&self, args: &[&str]) -> String {
        let output = host_git_output(&self.workspace, &self.home, args);
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn file(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.join(path)).ok()
    }
}

fn commit(paths: &[&str], message: &str) -> ToolCall {
    ToolCall::GitCommit {
        message: message.into(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        staged: false,
    }
}

fn branch(name: &str, start_point: Option<&str>) -> ToolCall {
    ToolCall::GitBranch {
        name: Some(name.into()),
        start_point: start_point.map(str::to_owned),
    }
}

fn switch(name: &str) -> ToolCall {
    ToolCall::GitSwitch {
        branch: name.into(),
    }
}

#[test]
fn commit_branch_and_switch_publish_exactly_what_their_receipts_name() {
    let mut repo = Repo::new();
    std::fs::write(repo.workspace.join("notes.md"), "staged notes\n").unwrap();
    host_git(&repo.workspace, &repo.home, &["add", "notes.md"]);
    let parent = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let index = std::fs::read(repo.workspace.join(".git/index")).unwrap();

    let committed = repo.succeeded(commit(&["src/a.txt", "src/new.txt"], "third\n\nbody"));
    let oid = committed["commit"].as_str().unwrap().to_owned();
    assert_eq!(committed["branch"], "main");
    let receipt = &committed["publication"];
    assert_eq!(
        receipt["ref"],
        json!({"name": "refs/heads/main", "before": parent, "after": oid})
    );
    assert_eq!(receipt["index"]["before"], hex_digest(&index));
    assert_eq!(
        receipt["index"]["after"],
        hex_digest(&std::fs::read(repo.workspace.join(".git/index")).unwrap())
    );
    assert!(receipt["objects"].as_u64().unwrap() >= 3, "{receipt}");
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), oid);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B%an <%ae>|%cn <%ce>"]),
        "third\n\nbody\nArany Writer <writer@example.invalid>|Arany Writer <writer@example.invalid>\n"
    );
    assert_eq!(
        repo.git(&["show", "--name-only", "--format=", "HEAD"]),
        "src/a.txt\nsrc/new.txt\n"
    );
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"]),
        "notes.md\n",
        "unrelated staging is preserved"
    );
    assert_eq!(
        repo.git(&["status", "--porcelain=v1", "--", "src"]),
        " M src/big.txt\n"
    );
    repo.git(&["fsck", "--connectivity-only", "--no-dangling"]);

    let nested = repo.succeeded(branch("feature/nested", None));
    assert_eq!(
        repo.git(&["rev-parse", "feature/nested"]).trim(),
        nested["oid"]
    );
    repo.git(&["branch", "-q", "-D", "feature/nested"]);
    let created = repo.succeeded(branch("older", Some("HEAD~1")));
    assert_eq!(created["oid"], parent);
    assert_eq!(repo.git(&["rev-parse", "older"]).trim(), parent);
    assert_eq!(
        repo.git(&["symbolic-ref", "HEAD"]),
        "refs/heads/main\n",
        "creating a branch never switches"
    );
    let listed = run_call(
        &repo.workspace,
        &scratch(repo.temp.path(), 100),
        &ToolCall::GitBranch {
            name: None,
            start_point: None,
        },
    );
    assert_eq!(
        listed,
        json!({"branches": [
            {"name": "main", "oid": oid, "current": true},
            {"name": "older", "oid": parent, "current": false},
        ], "truncated": false})
    );

    let switched = repo.succeeded(switch("older"));
    assert_eq!(switched["switched"], true);
    assert_eq!(switched["files"], 2);
    assert_eq!(
        switched["publication"]["head"],
        json!({"before": "ref: refs/heads/main", "after": "ref: refs/heads/older"})
    );
    assert_eq!(repo.file("src/a.txt").as_deref(), Some("bravo\n"));
    assert_eq!(repo.file("src/new.txt"), None);
    assert_eq!(repo.git(&["symbolic-ref", "HEAD"]), "refs/heads/older\n");
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "notes.md\n");
    assert_eq!(
        repo.git(&["status", "--porcelain=v1", "--", "src"]),
        " M src/big.txt\n"
    );

    repo.succeeded(switch("main"));
    assert_eq!(repo.file("src/a.txt").as_deref(), Some("delta\n"));
    assert_eq!(repo.file("src/new.txt").as_deref(), Some("new\n"));
    assert_eq!(repo.succeeded(switch("main"))["switched"], false);

    // "." commits every change except protected paths, which stay modified and uncommitted.
    // "." beside another path still commits everything.
    repo.succeeded(commit(&["notes.md", "."], "everything"));
    assert_eq!(
        repo.git(&["status", "--porcelain=v1", "--untracked-files=no"]),
        " M .env\n M .env.local\n M cfg/.Env\n"
    );
    assert_eq!(repo.git(&["show", "HEAD:.env"]), "SECRET_CANARY=2\n");
}

#[test]
fn hostile_repository_configuration_never_executes_during_writes() {
    let mut repo = Repo::new();
    let markers = arm_hostile(repo.temp.path(), &repo.workspace);
    let script = |path: PathBuf| {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(
            &path,
            format!("#!/bin/sh\n/usr/bin/touch {}/{name}\n", markers.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    for hook in [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "reference-transaction",
        "post-checkout",
        "post-index-change",
    ] {
        script(repo.temp.path().join("hooks").join(hook));
        script(repo.workspace.join(".git/hooks").join(hook));
    }
    let scripts = repo.temp.path().join("scripts");
    for name in ["ssh", "alias", "conditional-fsmonitor"] {
        script(scripts.join(name));
    }
    let conditional = repo.temp.path().join("conditional.config");
    std::fs::write(
        &conditional,
        format!(
            "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n",
            scripts.join("conditional-fsmonitor").display(),
            repo.workspace.join(".git/hooks").display()
        ),
    )
    .unwrap();
    {
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(repo.workspace.join(".git/config"))
            .unwrap();
        write!(
            config,
            "[core]\n\tsshCommand = {}\n[alias]\n\tci = !{}\n[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
            scripts.join("ssh").display(),
            scripts.join("alias").display(),
            repo.workspace.display(),
            conditional.display()
        )
        .unwrap();
    }

    repo.succeeded(commit(&["src/a.txt", ".gitattributes"], "hostile"));
    repo.succeeded(branch("topic", Some("HEAD~1")));
    assert_eq!(repo.succeeded(switch("topic"))["switched"], true);
    assert_eq!(repo.file("src/a.txt").as_deref(), Some("bravo\n"));
    assert_eq!(
        std::fs::read_dir(&markers).unwrap().count(),
        0,
        "no hook, filter, monitor, include or alias ran"
    );

    // Control: plain Git honors the same configuration and runs the hooks.
    host_git_output(
        &repo.workspace,
        &repo.home,
        &["commit", "--allow-empty", "-q", "-m", "control"],
    );
    assert!(markers.join("pre-commit").exists());
}

#[test]
fn refusals_leave_the_repository_and_workspace_unchanged() {
    type Setup = fn(&Repo);
    let symbolic: Setup = |repo| {
        let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
    };
    let cases: [(&str, Setup, ToolCall, &str); 21] = [
        (
            "a symbolic start point spelled with a tab",
            |repo| {
                host_git(
                    &repo.workspace,
                    &repo.home,
                    &["update-ref", "refs/remotes/origin/main", "HEAD~1"],
                );
                std::fs::write(
                    repo.workspace.join(".git/refs/remotes/origin/HEAD"),
                    "ref:\trefs/remotes/origin/main\n",
                )
                .unwrap();
            },
            branch("topic", Some("origin/HEAD")),
            SYMBOLIC_START,
        ),
        (
            "a branch from a HEAD outside local branches",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
                git(&["symbolic-ref", "HEAD", "refs/remotes/origin/main"]);
            },
            branch("topic", None),
            UNSUPPORTED_HEAD,
        ),
        (
            "switch from a detached HEAD",
            |repo| {
                host_git(&repo.workspace, &repo.home, &["branch", "other"]);
                host_git(&repo.workspace, &repo.home, &["checkout", "-q", "--detach"]);
            },
            switch("other"),
            DETACHED_SWITCH,
        ),
        (
            "a symbolic start point",
            symbolic,
            branch("topic", Some("origin/HEAD")),
            SYMBOLIC_START,
        ),
        (
            "a remote name resolving to its symbolic HEAD",
            symbolic,
            branch("topic", Some("origin")),
            SYMBOLIC_START,
        ),
        (
            "a branch differing from an existing one only by case",
            |_| {},
            branch("MAIN", None),
            REF_CASE_ALIAS,
        ),
        (
            "a branch directory differing from an existing one only by case",
            |repo| host_git(&repo.workspace, &repo.home, &["branch", "feature/y"]),
            branch("Feature/x", None),
            REF_CASE_ALIAS,
        ),
        (
            "new parent directories differing only by case",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["checkout", "-q", "-b", "parents"]);
                for path in ["Src2/x.txt", "src2/y.txt"] {
                    let path = repo.workspace.join(path);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "x\n").unwrap();
                }
                git(&["add", "Src2/x.txt", "src2/y.txt"]);
                git(&["commit", "-q", "-m", "parents"]);
                git(&["checkout", "-q", "main"]);
            },
            switch("parents"),
            CASE_ALIAS,
        ),
        (
            "an added path under a case alias of an existing directory",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["checkout", "-q", "-b", "upper"]);
                std::fs::create_dir_all(repo.workspace.join("SRC")).unwrap();
                std::fs::write(repo.workspace.join("SRC/z.txt"), "z\n").unwrap();
                git(&["add", "SRC/z.txt"]);
                git(&["commit", "-q", "-m", "upper"]);
                git(&["checkout", "-q", "main"]);
            },
            switch("upper"),
            CASE_ALIAS,
        ),
        (
            "detached HEAD",
            |repo| host_git(&repo.workspace, &repo.home, &["checkout", "-q", "--detach"]),
            commit(&["src/a.txt"], "m"),
            DETACHED,
        ),
        (
            "merge in progress",
            |repo| {
                let head = repo.git(&["rev-parse", "HEAD"]);
                std::fs::write(repo.workspace.join(".git/MERGE_HEAD"), head).unwrap();
            },
            commit(&["src/a.txt"], "m"),
            IN_PROGRESS_MESSAGE,
        ),
        (
            "switch over a local change",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["stash", "-q"]);
                git(&["checkout", "-q", "-b", "other"]);
                std::fs::write(repo.workspace.join("src/a.txt"), "other\n").unwrap();
                git(&["commit", "-q", "-am", "other"]);
                git(&["checkout", "-q", "main"]);
                git(&["stash", "pop", "-q"]);
            },
            switch("other"),
            "untracked files in 1 path(s): src/a.txt",
        ),
        (
            "switch changing a protected path",
            |repo| host_git(&repo.workspace, &repo.home, &["branch", "older", "HEAD~1"]),
            switch("older"),
            PROTECTED,
        ),
        (
            "switch over an untracked file",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["checkout", "-q", "-b", "adds"]);
                git(&["add", "src/new.txt"]);
                git(&["commit", "-q", "-m", "adds"]);
                git(&["checkout", "-q", "main"]);
                std::fs::write(repo.workspace.join("src/new.txt"), "mine\n").unwrap();
            },
            switch("adds"),
            "untracked files in 1 path(s): src/new.txt",
        ),
        (
            "paths differing only by case",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["checkout", "-q", "-b", "cased"]);
                git(&["mv", "notes.md", "NOTES.md"]);
                git(&["commit", "-q", "-m", "cased"]);
                git(&["checkout", "-q", "main"]);
            },
            switch("cased"),
            CASE_ALIAS,
        ),
        (
            "an added path aliasing an existing name by case",
            |repo| {
                let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
                git(&["checkout", "-q", "-b", "aliased"]);
                std::fs::write(repo.workspace.join("src/NEW.txt"), "upper\n").unwrap();
                git(&["add", "src/NEW.txt"]);
                git(&["commit", "-q", "-m", "aliased"]);
                git(&["checkout", "-q", "main"]);
            },
            switch("aliased"),
            CASE_ALIAS,
        ),
        (
            "existing branch",
            |_| {},
            branch("main", None),
            "already exists",
        ),
        ("missing branch", |_| {}, switch("nope"), "No local branch"),
        (
            "nothing staged",
            |_| {},
            ToolCall::GitCommit {
                message: "m".into(),
                paths: Vec::new(),
                staged: true,
            },
            "git exited with status 1",
        ),
        (
            "index locked by another Git",
            |repo| std::fs::write(repo.workspace.join(".git/index.lock"), "").unwrap(),
            commit(&["src/a.txt"], "m"),
            DRIFT,
        ),
        (
            "branch locked by another Git",
            |repo| std::fs::write(repo.workspace.join(".git/refs/heads/main.lock"), "").unwrap(),
            commit(&["src/a.txt"], "m"),
            DRIFT,
        ),
    ];
    for (name, setup, call, expected) in cases {
        let mut repo = Repo::new();
        setup(&repo);
        let before = tree(&repo.workspace);
        let value = repo.write(call).expect(name);
        assert!(
            value["failed"]
                .as_str()
                .is_some_and(|failed| failed.contains(expected)),
            "{name}: {value}"
        );
        assert_unchanged(&repo.workspace, &before);
    }
}

#[test]
fn a_failure_after_the_first_published_effect_is_uncertain() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let mut repo = Repo::new();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let objects = repo.workspace.join(".git/objects");
    let mut restore = Vec::new();
    for directory in std::fs::read_dir(&objects)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .chain([objects.clone()])
    {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        restore.push(RestoreMode(directory));
    }
    assert!(matches!(
        repo.write(commit(&["src/a.txt"], "m")),
        Err(ToolError::Uncertain)
    ));
    drop(restore);
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]),
        head,
        "the ref never moved"
    );
    for lock in ["index.lock", "refs/heads/main.lock"] {
        assert!(!repo.workspace.join(".git").join(lock).exists(), "{lock}");
    }
}

#[test]
fn concurrent_ref_changes_after_the_snapshot_publish_nothing() {
    type Setup = fn(&Repo);
    type Case = (
        &'static str,
        Setup,
        Stage,
        &'static [&'static [&'static str]],
        ToolCall,
    );
    let cases: [Case; 7] = [
        (
            "nested branch while its source advanced",
            |_| {},
            Stage::Planned,
            &[&["commit", "-q", "--allow-empty", "-m", "concurrent"]],
            branch("feature/x", None),
        ),
        (
            "branch advanced after the snapshot",
            |_| {},
            Stage::Snapshot,
            &[&["commit", "-q", "--allow-empty", "-m", "concurrent"]],
            commit(&["src/a.txt"], "m"),
        ),
        (
            "branch advanced before publication",
            |_| {},
            Stage::Planned,
            &[&["commit", "-q", "--allow-empty", "-m", "concurrent"]],
            commit(&["src/a.txt"], "m"),
        ),
        (
            "HEAD moved before a commit publishes",
            |repo| host_git(&repo.workspace, &repo.home, &["branch", "other"]),
            Stage::Planned,
            &[&["symbolic-ref", "HEAD", "refs/heads/other"]],
            commit(&["src/a.txt"], "m"),
        ),
        (
            "switch target advanced",
            |repo| host_git(&repo.workspace, &repo.home, &["branch", "other"]),
            Stage::Planned,
            &[&["update-ref", "refs/heads/other", "HEAD~1"]],
            switch("other"),
        ),
        (
            "switch source advanced",
            |repo| host_git(&repo.workspace, &repo.home, &["branch", "other"]),
            Stage::Planned,
            &[&["commit", "-q", "--allow-empty", "-m", "concurrent"]],
            switch("other"),
        ),
        (
            "refs packed before a branch publishes",
            |_| {},
            Stage::Planned,
            &[&["pack-refs", "--all"]],
            branch("topic", None),
        ),
    ];
    for (name, setup, stage, race, call) in cases {
        let mut repo = Repo::new();
        setup(&repo);
        arm(&repo, stage, race);
        let value = repo.write(call).expect(name);
        assert_eq!(value["failed"], DRIFT, "{name}: {value}");
        let after_race = tree(&repo.workspace);
        assert!(
            !after_race
                .iter()
                .any(|(path, _)| path.extension().is_some_and(|e| e == "lock")),
            "{name}: no lock remains"
        );
        assert!(
            repo.git(&["log", "--format=%s", "main"])
                .starts_with("concurrent")
                || !name.contains("advanced")
                || name.contains("target"),
            "{name}: the concurrent commit is never overwritten"
        );
        for created in ["topic", "feature"] {
            assert!(
                !repo
                    .workspace
                    .join(".git/refs/heads")
                    .join(created)
                    .exists(),
                "{name}: nothing was created"
            );
        }
    }
}

#[test]
fn an_edit_racing_the_switch_cleanliness_check_is_never_overwritten() {
    let mut repo = Repo::new();
    {
        let git = |args: &[&str]| host_git(&repo.workspace, &repo.home, args);
        git(&["stash", "-q"]);
        git(&["checkout", "-q", "-b", "other"]);
        std::fs::write(repo.workspace.join("src/a.txt"), "other\n").unwrap();
        git(&["commit", "-q", "-am", "other"]);
        git(&["checkout", "-q", "main"]);
    }
    assert_eq!(repo.file("src/a.txt").as_deref(), Some("bravo\n"));
    let path = repo.workspace.join("src/a.txt");
    RACES.lock().unwrap().push((
        repo.workspace.clone(),
        Stage::Checked,
        Box::new(move || std::fs::write(path, "racing edit\n").unwrap()),
    ));
    let value = repo.write(switch("other")).unwrap();
    assert!(
        value["failed"]
            .as_str()
            .is_some_and(|failed| failed.contains("1 path(s): src/a.txt")),
        "{value}"
    );
    assert_eq!(repo.file("src/a.txt").as_deref(), Some("racing edit\n"));
    assert_eq!(repo.git(&["symbolic-ref", "HEAD"]), "refs/heads/main\n");
}

#[tokio::test]
async fn the_git_child_cannot_write_the_workspace_directly() {
    let repo = Repo::new();
    let scratch = scratch(repo.temp.path(), 0);
    let git = super::super::discover().expect("pinned system git");
    // Without GIT_DIR, `git init TARGET` writes TARGET/.git itself.
    let env: Vec<_> = super::super::environment(&scratch.join(VIEW), &repo.workspace, &scratch)
        .into_iter()
        .filter(|(key, _)| !key.starts_with("GIT_DIR") && *key != "GIT_WORK_TREE")
        .collect();
    let before = tree(&repo.workspace);
    // `git config --file` creates its file through a lock and a rename, as a write's Git does.
    let init = |target: PathBuf| {
        let argv = vec![
            "config".to_owned(),
            "--file".to_owned(),
            target.display().to_string(),
            "arany.probe".to_owned(),
            "yes".to_owned(),
        ];
        let env = env.clone();
        let git = git.clone();
        let workspace = repo.workspace.clone();
        let scratch = scratch.clone();
        async move {
            super::super::spawn_git_with(
                Path::new(&git.executable),
                &argv,
                &env,
                &workspace,
                tokio::time::Instant::now() + Duration::from_secs(20),
                super::super::Io {
                    stdin: None,
                    stdout_bytes: CAPTURE_BYTES,
                    writable: Some(&scratch),
                },
            )
            .await
            .expect("confined Git spawns")
        }
    };
    let denied = init(repo.workspace.join(".git/injected")).await;
    assert_ne!(denied.status, Some(0));
    assert_unchanged(&repo.workspace, &before);
    let allowed = init(scratch.join("private.config")).await;
    assert_eq!(allowed.status, Some(0));
    assert!(scratch.join("private.config").exists());
}

#[test]
fn a_branch_start_ref_stays_locked_until_the_branch_publishes() {
    let mut repo = Repo::new();
    host_git(&repo.workspace, &repo.home, &["branch", "older", "HEAD~1"]);
    let older = repo.git(&["rev-parse", "older"]);
    let moved = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (workspace, home, flag) = (repo.workspace.clone(), repo.home.clone(), moved.clone());
    RACES.lock().unwrap().push((
        repo.workspace.clone(),
        Stage::Locked,
        Box::new(move || {
            let output = host_git_output(
                &workspace,
                &home,
                &["update-ref", "refs/heads/older", "HEAD"],
            );
            flag.store(output.status.success(), std::sync::atomic::Ordering::SeqCst);
        }),
    ));
    repo.succeeded(branch("topic", Some("older")));
    assert!(
        !moved.load(std::sync::atomic::Ordering::SeqCst),
        "the start ref was held"
    );
    assert_eq!(repo.git(&["rev-parse", "older"]), older);
    assert_eq!(repo.git(&["rev-parse", "topic"]), older);
}

#[test]
fn locks_left_by_a_dead_guard_are_reported_and_never_touched() {
    let mut repo = Repo::new();
    host_git(&repo.workspace, &repo.home, &["branch", "older", "HEAD~1"]);
    let stash = repo.temp.path().join("stash");
    std::fs::create_dir(&stash).unwrap();
    let left = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (git_dir, stash_dir, kept) = (repo.workspace.join(".git"), stash.clone(), left.clone());
    // Hard links keep each lock's inode and bytes, as a killed Guard would leave them.
    RACES.lock().unwrap().push((
        repo.workspace.clone(),
        Stage::Locked,
        Box::new(move || {
            for (index, (path, _)) in tree(&git_dir).into_iter().enumerate() {
                let name = path.to_string_lossy().into_owned();
                if name.ends_with(".lock") || name.starts_with("arany-locks-") {
                    let link = stash_dir.join(index.to_string());
                    std::fs::hard_link(git_dir.join(&path), &link).unwrap();
                    kept.lock().unwrap().push((path, link));
                }
            }
        }),
    ));
    repo.succeeded(branch("topic", Some("older")));
    let git_dir = repo.workspace.join(".git");
    let residue = |git_dir: &Path| {
        tree(git_dir)
            .into_iter()
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .filter(|name| name.contains(".lock") || name.starts_with("arany-"))
            .collect::<BTreeSet<_>>()
    };
    assert!(
        residue(&git_dir).is_empty(),
        "a finished write leaves no lock or record"
    );
    let left = left.lock().unwrap().clone();
    let ledger = format!("arany-locks-{}.json", repo.intent);
    assert_eq!(
        left.iter()
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>(),
        [
            "HEAD.lock".to_owned(),
            ledger.clone(),
            "packed-refs.lock".into(),
            "refs/heads/older.lock".into(),
            "refs/heads/topic.lock".into(),
        ]
        .into_iter()
        .collect()
    );
    // The renamed-into-place branch would not survive a crash as a lock; the rest return.
    for (path, link) in &left {
        if path != Path::new("refs/heads/topic.lock") {
            std::fs::hard_link(link, git_dir.join(path)).unwrap();
            std::fs::remove_file(link).unwrap();
        }
    }
    // A lock replaced by another Git is not ours and is not reported.
    std::fs::remove_file(git_dir.join("HEAD.lock")).unwrap();
    std::fs::write(git_dir.join("HEAD.lock"), "").unwrap();
    let before = tree(&git_dir);
    recover_locks(&repo.workspace, uuid::Uuid::now_v7()).unwrap();
    // Exclusion over a dead Guard's locks cannot be proven, so they are reported, never touched.
    let reported = recover_locks(&repo.workspace, repo.intent);
    assert!(
        matches!(&reported, Err(ToolError::StaleGitLocks(paths))
            if paths == ".git/refs/heads/older.lock, .git/packed-refs.lock"),
        "{reported:?}"
    );
    assert_unchanged(&git_dir, &before);
    for path in ["HEAD.lock", "packed-refs.lock", "refs/heads/older.lock"] {
        std::fs::remove_file(git_dir.join(path)).unwrap();
    }
    recover_locks(&repo.workspace, repo.intent).unwrap();
    assert!(
        residue(&git_dir).is_empty(),
        "the record goes once its locks are gone"
    );
    assert_eq!(
        repo.git(&["rev-parse", "topic"]),
        repo.git(&["rev-parse", "older"])
    );
}

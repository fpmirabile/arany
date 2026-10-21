use super::super::tests::{
    RestoreMode, arm_hostile, assert_unchanged, fixture, host_git, host_git_output, run_call,
    scratch, tree,
};
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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
        }
    }

    fn write(&mut self, call: ToolCall) -> Result<Value, ToolError> {
        self.runs += 1;
        let git = super::super::discover().expect("pinned system git");
        let scratch = scratch(self.temp.path(), self.runs);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(execute(
                &self.workspace,
                &scratch,
                &git,
                &identity(),
                &call,
                16 * 1024,
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
    repo.succeeded(commit(&["."], "everything"));
    assert_eq!(
        repo.git(&["status", "--porcelain=v1", "--untracked-files=no"]),
        " M .Env\n M .env\n M .env.local\n"
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
    let cases: [(&str, Setup, ToolCall, &str); 10] = [
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
fn user_identity_reads_only_the_user_section_as_data() {
    for (text, expected) in [
        (
            "[user]\n\tname = Ana Lima\n\temail = ana@example.com\n",
            (Some("Ana Lima"), Some("ana@example.com")),
        ),
        (
            "[User]\nName = \"Ana # Lima\"\nemail = ana@example.com ; work\n",
            (Some("Ana # Lima"), Some("ana@example.com")),
        ),
        (
            "[user \"x\"]\n\tname = sub\n[core]\n\tname = other\n",
            (None, None),
        ),
        (
            "[include]\n\tpath = elsewhere\n[user]\n\tname = \"esc\\\"aped\"\n",
            (None, None),
        ),
    ] {
        let (name, email) = super::super::user_section(text);
        assert_eq!((name.as_deref(), email.as_deref()), expected, "{text}");
    }
    assert!(GitIdentity::default().valid());
    for (name, email) in [
        ("", "a@b"),
        ("A <b>", "a@b"),
        ("A", "a b@c"),
        ("A\n", "a@b"),
    ] {
        assert!(
            !GitIdentity {
                name: name.into(),
                email: email.into()
            }
            .valid()
        );
    }
}

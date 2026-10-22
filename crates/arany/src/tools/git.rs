pub(crate) mod host;
mod write;

pub(crate) use write::recover_locks;

use super::ToolError;
use super::config::{GitIdentity, Program};
use super::executable::{EXECUTABLES, git_root, has_magic};
use super::guard::GuardContext;
use super::permissions::HiddenPaths;
use super::types::{
    FORBIDDEN_NAMES, MAX_GIT_LOG_COMMITS, MAX_SNAPSHOT_BYTES, ToolCall, forbidden, hex_digest,
    valid_relative, valid_revision,
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::GitStatus { path } => valid_relative(path, true),
        ToolCall::GitDiff {
            path,
            base,
            head,
            staged,
        } => {
            valid_relative(path, true)
                && base.as_deref().is_none_or(valid_revision)
                && head.as_deref().is_none_or(valid_revision)
                && (head.is_none() || base.is_some() && !staged)
        }
        ToolCall::GitLog {
            path,
            revision,
            max_count,
        } => {
            valid_relative(path, true)
                && revision.as_deref().is_none_or(valid_revision)
                && (1..=MAX_GIT_LOG_COMMITS).contains(max_count)
        }
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => {
            valid_message(message)
                && if *staged {
                    paths.is_empty()
                } else {
                    (1..=MAX_COMMIT_PATHS).contains(&paths.len())
                        && paths
                            .iter()
                            .all(|path| path == "." || valid_relative(path, false))
                        && paths
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            == paths.len()
                }
        }
        ToolCall::GitBranch {
            name: None,
            start_point,
        } => start_point.is_none(),
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
        } => valid_branch(name) && start_point.as_deref().is_none_or(valid_revision),
        ToolCall::GitSwitch { branch } => valid_branch(branch),
        call if call.is_host() => host::valid(call),
        _ => false,
    }
}

const MAX_COMMIT_PATHS: usize = 128;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;

fn valid_message(message: &str) -> bool {
    message.len() <= MAX_MESSAGE_BYTES
        && !message.trim().is_empty()
        && !message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
}

// A local branch name: the revision name grammar without ancestry steps, never HEAD itself.
pub(super) fn valid_branch(name: &str) -> bool {
    valid_revision(name) && !name.contains(['~', '^']) && name != "HEAD"
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    let path = (
        "path",
        json!({"type":"string", "description":"Workspace-relative path that limits the result; \".\" for the whole repository."}),
    );
    let revision = json!({"type":["string","null"], "description":"null, or HEAD, a branch or tag name, or a 7-64 character hex commit name, with at most four ~N or ^N ancestry steps (N at most 9999). No ranges, ':' or '@{'. At most 128 bytes."});
    Some(match operation {
        "git_status" => vec![path],
        "git_diff" => vec![
            path,
            ("base", revision.clone()),
            ("head", revision),
            (
                "staged",
                json!({"type":"boolean", "description":"true compares the index with base, or HEAD when base is null; head must then be null. false compares the working tree with the index (no revisions), with base, or compares base with head."}),
            ),
        ],
        "git_log" => vec![
            path,
            ("revision", revision),
            (
                "max_count",
                json!({"type":"integer", "description":"Number of commits: integer 1 through 50 inclusive."}),
            ),
        ],
        "git_commit" => vec![
            (
                "message",
                json!({"type":"string", "description":"Commit message: non-blank UTF-8, at most 8192 bytes; newlines and tabs allowed, no other control characters."}),
            ),
            (
                "paths",
                json!({"type":"array", "items":{"type":"string"}, "description":"Up to 128 distinct Workspace-relative literal paths (\".\" for every change) whose working-tree contents to commit; empty when staged is true."}),
            ),
            (
                "staged",
                json!({"type":"boolean", "description":"true commits exactly the staged index and requires empty paths."}),
            ),
        ],
        "git_branch" => vec![
            (
                "name",
                json!({"type":["string","null"], "description":"null lists local branches; otherwise the new branch name: letters, digits, '.', '_', '-' and '/' separators, at most 128 bytes."}),
            ),
            ("start_point", revision),
        ],
        "git_switch" => vec![(
            "branch",
            json!({"type":"string", "description":"Existing local branch name."}),
        )],
        _ => return host::schema(operation),
    })
}

const CAPTURE_BYTES: usize = 256 * 1024;
const STDERR_BYTES: usize = 4 * 1024;
const ERROR_BYTES: usize = 1024;
const MAX_ROWS: usize = 256;
const MAX_FILTERED_LOG_COMMITS: u32 = 1000;
const ENVELOPE_BYTES: usize = 1024;
const HEAD_BYTES: usize = 256;
const CONFIG_BYTES: usize = 64 * 1024;
const MAX_GIT_RUNTIME: Duration = Duration::from_millis(super::types::GIT_TIMEOUT_MS as u64);
pub(super) const VIEW: &str = "git-view";
pub(super) const HOME: &str = "git-home";
const VIEW_CONFIG: &[u8] = b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfsmonitor = false\n\thooksPath = /dev/null\n\tuntrackedCache = false\n\tquotePath = false\n";
const LINKED: [(&str, bool, bool); 4] = [
    ("objects", true, true),
    ("refs", true, true),
    ("packed-refs", false, false),
    ("shallow", false, false),
];
const NO_REPOSITORY: &str = "The Workspace root has no .git directory; typed Git reads need a repository rooted at the Workspace.";
const UNSUPPORTED_LAYOUT: &str = "The Workspace .git is not a plain repository directory (linked worktree, submodule checkout, gitfile, symlink or missing data); typed Git reads do not support it.";
const UNSUPPORTED_FORMAT: &str = "The repository uses Git extensions (such as SHA-256 objects, reftable refs or partial clone) that typed Git reads do not support.";

fn executable_bytes(executable: &str) -> Result<Vec<u8>, ToolError> {
    use std::os::unix::fs::MetadataExt;
    let file = super::fs::open_absolute(executable)?;
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_file()
        || metadata.mode() & 0o6022 != 0
        || metadata.len() > MAX_SNAPSHOT_BYTES as u64
    {
        return Err(ToolError::ChangedInput);
    }
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::Operation)?;
    if bytes.len() > MAX_SNAPSHOT_BYTES || !has_magic(&bytes, EXECUTABLES.git_magics) {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

fn verify_pin(git: &Program) -> Result<(), ToolError> {
    if hex_digest(&executable_bytes(&git.executable)?) != git.sha256 {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

// Check root ownership on the host because root appears unmapped inside the user namespace.
pub(super) fn verify_host_executable(git: &Program) -> Result<(), ToolError> {
    use std::os::unix::fs::MetadataExt;
    let metadata =
        std::fs::symlink_metadata(&git.executable).map_err(|_| ToolError::ChangedInput)?;
    if metadata.uid() != 0 {
        return Err(ToolError::ChangedInput);
    }
    verify_pin(git)
}

pub(super) fn discover() -> Option<Program> {
    EXECUTABLES.git.iter().find_map(|candidate| {
        let executable = std::fs::canonicalize(candidate.path).ok()?;
        if !git_root(&executable) {
            return None;
        }
        let executable = executable.to_str()?.to_owned();
        let sha256 = hex_digest(&executable_bytes(&executable).ok()?);
        let program = Program {
            name: "git".into(),
            executable,
            sha256,
            interpreter: false,
            inputs: Vec::new(),
        };
        verify_host_executable(&program).ok()?;
        Some(program)
    })
}

const USER_CONFIG_BYTES: u64 = 64 * 1024;

fn read_user_config(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .ok()
        .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))?;
    let mut bytes = Vec::new();
    file.take(USER_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= USER_CONFIG_BYTES).then_some(bytes)
}

// Parsed as data only: includes and every key except user.name/user.email are ignored.
pub(super) fn user_identity() -> Option<GitIdentity> {
    let home = directories::BaseDirs::new()?.home_dir().to_owned();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    // Git reads the XDG file first, so ~/.gitconfig wins for each key.
    let (mut name, mut email) = (None, None);
    for path in [xdg.join("git/config"), home.join(".gitconfig")] {
        let Some(bytes) = read_user_config(&path) else {
            continue;
        };
        let (found_name, found_email) = user_section(&String::from_utf8_lossy(&bytes));
        name = found_name.or(name);
        email = found_email.or(email);
    }
    let identity = GitIdentity {
        name: name?,
        email: email?,
    };
    identity.valid().then_some(identity)
}

fn user_section(text: &str) -> (Option<String>, Option<String>) {
    let (mut name, mut email) = (None, None);
    let mut in_user = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            in_user = header
                .split_once(']')
                .is_some_and(|(section, _)| section.trim().eq_ignore_ascii_case("user"));
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_user) else {
            continue;
        };
        let value = value.trim();
        let value = match value.strip_prefix('"') {
            Some(quoted) => quoted.strip_suffix('"'),
            None => Some(
                value
                    .split(['#', ';'])
                    .next()
                    .unwrap_or_default()
                    .trim_end(),
            ),
        };
        // Escapes would need Git's full value grammar; such a value is ignored.
        let Some(value) = value.filter(|value| !value.contains(['\\', '"'])) else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(value.to_owned()),
            "email" => email = Some(value.to_owned()),
            _ => {}
        }
    }
    (name, email)
}

enum Prepared {
    Ready,
    Unsupported(&'static str),
}

struct Repository {
    git_dir: Dir,
    identity: (u64, u64),
    head: Vec<u8>,
    index: Option<(Vec<u8>, std::time::SystemTime)>,
    links: Vec<&'static str>,
}

fn inspect_layout(workspace: &Path) -> Result<Result<Repository, &'static str>, ToolError> {
    let root = super::fs::open_directory(workspace)?;
    match root.symlink_metadata(".git") {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(Err(UNSUPPORTED_LAYOUT)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(NO_REPOSITORY));
        }
        Err(_) => return Err(ToolError::Path),
    }
    let git_dir = root
        .open_dir_nofollow(".git")
        .map_err(|_| ToolError::Path)?;
    // An oversized, linked, multi-link or special HEAD/config is an unsupported layout, not a Tool fault.
    let head = match read_optional(&git_dir, "HEAD", HEAD_BYTES) {
        Ok(Some(head)) => head,
        Ok(None) | Err(ToolError::Limit | ToolError::Path) => {
            return Ok(Err(UNSUPPORTED_LAYOUT));
        }
        Err(error) => return Err(error),
    };
    if !valid_head(&head) {
        return Ok(Err(UNSUPPORTED_FORMAT));
    }
    match read_optional(&git_dir, "config", CONFIG_BYTES) {
        Ok(Some(config)) if uses_extensions(&config) => {
            return Ok(Err(UNSUPPORTED_FORMAT));
        }
        Ok(_) => {}
        Err(ToolError::Limit | ToolError::Path) => {
            return Ok(Err(UNSUPPORTED_LAYOUT));
        }
        Err(error) => return Err(error),
    }
    let mut links = Vec::with_capacity(LINKED.len());
    for (name, directory, required) in LINKED {
        match git_dir.symlink_metadata(name) {
            Ok(metadata)
                if (directory && metadata.is_dir()) || (!directory && metadata.is_file()) =>
            {
                links.push(name);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {}
            _ => return Ok(Err(UNSUPPORTED_LAYOUT)),
        }
    }
    Ok(Ok(Repository {
        identity: super::fs::identity(&git_dir)?,
        git_dir,
        head,
        index: None,
        links,
    }))
}

fn inspect(workspace: &Path) -> Result<Result<Repository, &'static str>, ToolError> {
    let mut repository = match inspect_layout(workspace)? {
        Ok(repository) => repository,
        Err(reason) => return Ok(Err(reason)),
    };
    repository.index = match open_optional(&repository.git_dir, "index").and_then(|file| {
        file.map(|file| {
            let modified = file
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|_| ToolError::Path)?;
            Ok((super::fs::read_regular(file, MAX_SNAPSHOT_BYTES)?, modified))
        })
        .transpose()
    }) {
        Ok(index) => index,
        Err(ToolError::Path) => return Ok(Err(UNSUPPORTED_LAYOUT)),
        Err(error) => return Err(error),
    };
    Ok(Ok(repository))
}

// The fixed config, HEAD and index copies every view starts from.
fn write_view_base(repository: &Repository, view: &Path, config: &[u8]) -> Result<(), ToolError> {
    std::fs::create_dir(view).map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("config"), config).map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("HEAD"), &repository.head).map_err(|_| ToolError::Operation)?;
    if let Some((index, modified)) = &repository.index {
        // Git's racy-stat check compares entry times with the index mtime; a fresh mtime hides same-size edits.
        std::fs::File::create_new(view.join("index"))
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, index)?;
                file.set_modified(*modified)
            })
            .map_err(|_| ToolError::Operation)?;
    }
    Ok(())
}

fn prepare_view(workspace: &Path, view: &Path) -> Result<Prepared, ToolError> {
    let repository = match inspect(workspace)? {
        Ok(repository) => repository,
        Err(message) => return Ok(Prepared::Unsupported(message)),
    };
    write_view_base(&repository, view, VIEW_CONFIG)?;
    for name in &repository.links {
        std::os::unix::fs::symlink(workspace.join(".git").join(name), view.join(name))
            .map_err(|_| ToolError::Operation)?;
    }
    Ok(Prepared::Ready)
}

fn read_optional(git_dir: &Dir, name: &str, cap: usize) -> Result<Option<Vec<u8>>, ToolError> {
    open_optional(git_dir, name)?
        .map(|file| super::fs::read_regular(file, cap))
        .transpose()
}

fn open_optional(git_dir: &Dir, name: &str) -> Result<Option<std::fs::File>, ToolError> {
    use rustix::fs::{Mode, OFlags, openat};
    match openat(
        git_dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(std::fs::File::from(fd))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(ToolError::Path),
    }
}

fn valid_head(head: &[u8]) -> bool {
    let head = head.strip_suffix(b"\n").unwrap_or(head);
    head.strip_prefix(b"ref: refs/")
        .is_some_and(|name| !name.is_empty() && name.iter().all(u8::is_ascii_graphic))
        || head.len() == 40
            && head
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn uses_extensions(config: &[u8]) -> bool {
    String::from_utf8_lossy(config)
        .trim_start_matches('\u{feff}')
        .lines()
        .any(|line| {
            line.trim_start()
                .to_ascii_lowercase()
                .starts_with("[extensions")
        })
}

fn pathspecs(path: &str) -> Vec<String> {
    pathspecs_of([path])
}

fn pathspecs_of<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut specs: Vec<String> = paths
        .into_iter()
        .filter(|path| !path.is_empty() && *path != ".")
        .map(|path| format!(":(literal){path}"))
        .collect();
    for name in FORBIDDEN_NAMES.into_iter().chain([".env.*"]) {
        specs.push(format!(":(icase,exclude,glob)**/{name}"));
        specs.push(format!(":(icase,exclude,glob)**/{name}/**"));
    }
    specs
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn invocations(call: &ToolCall, filtered: bool) -> Result<Vec<Vec<String>>, ToolError> {
    let mut runs = Vec::new();
    match call {
        ToolCall::GitStatus { path } => {
            // No revisions are passed here, and `--` already ends options, so there is no `--end-of-options`.
            let mut argv = owned(&[
                "--no-pager",
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=normal",
                "--ignore-submodules=all",
                "--no-renames",
                "--",
            ]);
            argv.extend(pathspecs(path));
            runs.push(argv);
        }
        ToolCall::GitDiff {
            path,
            base,
            head,
            staged,
        } => {
            for mode in [
                &["--numstat", "-z"][..],
                &["--patch", "--unified=3", "--submodule=short"][..],
            ] {
                let mut argv = owned(&["--no-pager", "--no-optional-locks", "diff"]);
                argv.extend(owned(mode));
                argv.extend(owned(&[
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--ignore-submodules=all",
                ]));
                if *staged {
                    argv.push("--cached".into());
                }
                argv.push("--end-of-options".into());
                argv.extend(base.iter().chain(head.iter()).cloned());
                argv.push("--".into());
                argv.extend(pathspecs(path));
                runs.push(argv);
            }
        }
        ToolCall::GitLog {
            path,
            revision,
            max_count,
        } => {
            let mut argv = owned(&[
                "--no-pager",
                "--no-optional-locks",
                "log",
                "--no-color",
                "--no-show-signature",
            ]);
            if filtered {
                // Rename detection reports only the destination, which would hide a visible source.
                argv.extend(owned(&[
                    "--name-only",
                    "--no-renames",
                    "--diff-merges=first-parent",
                ]));
                argv.push(format!("--max-count={MAX_FILTERED_LOG_COMMITS}"));
            } else {
                argv.push(format!("--max-count={max_count}"));
            }
            argv.push("--format=tformat:%H%x00%P%x00%an%x00%at%x00%s".into());
            argv.push("--end-of-options".into());
            argv.extend(revision.iter().cloned());
            argv.push("--".into());
            argv.extend(pathspecs(path));
            runs.push(argv);
        }
        ToolCall::GitBranch { name: None, .. } => runs.push(owned(&[
            "--no-pager",
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(HEAD)",
            "refs/heads/",
        ])),
        _ => return Err(ToolError::Configuration),
    }
    Ok(runs)
}

fn environment(view: &Path, workspace: &Path, scratch: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("GIT_DIR", view.into()),
        ("GIT_WORK_TREE", workspace.into()),
        ("HOME", scratch.join(HOME).into_os_string()),
        ("PATH", "/usr/bin:/bin".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_ATTR_NOSYSTEM", "1".into()),
        ("GIT_OPTIONAL_LOCKS", "0".into()),
        ("GIT_NO_REPLACE_OBJECTS", "1".into()),
        ("GIT_NO_LAZY_FETCH", "1".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("LC_ALL", "C".into()),
        ("TZ", "UTC".into()),
    ]
}

struct Captured {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    overflow: bool,
}

// Drain excess stderr to avoid killing a verbose writer; cut stdout off to stop overlong output.
async fn read_prefix(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    cap: usize,
    drain: bool,
) -> Result<(Vec<u8>, bool), ToolError> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ToolError::Operation)?;
    let overflow = bytes.len() > cap;
    bytes.truncate(cap);
    if overflow && drain {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    }
    Ok((bytes, overflow))
}

async fn spawn_git(
    git: &Path,
    argv: &[String],
    env: &[(&'static str, OsString)],
    workspace: &Path,
    deadline: tokio::time::Instant,
) -> Result<Captured, ToolError> {
    spawn_git_with(git, argv, env, workspace, deadline, Io::default()).await
}

// Optional stdin, the stdout capture bound, and the only path a write's Git child may modify.
struct Io<'a> {
    stdin: Option<&'a [u8]>,
    stdout_bytes: usize,
    writable: Option<&'a Path>,
}

impl Default for Io<'_> {
    fn default() -> Self {
        Self {
            stdin: None,
            stdout_bytes: CAPTURE_BYTES,
            writable: None,
        }
    }
}

// A write's Git child may modify only `writable` (its private view); publication stays with the Guard.
fn spawn_confined_before(
    command: &mut tokio::process::Command,
    writable: &Path,
    deadline: tokio::time::Instant,
) -> Result<tokio::process::Child, ToolError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::Timeout(String::new()));
    }
    #[cfg(target_os = "linux")]
    {
        super::guard::spawn_confined(command, &[writable, Path::new("/dev/null")])
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, writable);
        Err(ToolError::ProtectionUnavailable)
    }
}

async fn spawn_git_with(
    git: &Path,
    argv: &[String],
    env: &[(&'static str, OsString)],
    workspace: &Path,
    deadline: tokio::time::Instant,
    io: Io<'_>,
) -> Result<Captured, ToolError> {
    let Io {
        stdin,
        stdout_bytes,
        writable,
    } = io;
    let mut command = tokio::process::Command::new(git);
    command
        .args(argv)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(workspace)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match writable {
        Some(writable) => spawn_confined_before(&mut command, writable, deadline)?,
        None => super::guard::spawn_before(&mut command, deadline)?,
    };
    let input = child.stdin.take();
    let stdout = child.stdout.take().ok_or(ToolError::Operation)?;
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let result = tokio::time::timeout_at(deadline, async {
        // Feed stdin while draining stdout, or a full pipe deadlocks both sides.
        let feed = async {
            if let (Some(mut input), Some(bytes)) = (input, stdin) {
                tokio::io::AsyncWriteExt::write_all(&mut input, bytes)
                    .await
                    .map_err(|_| ToolError::Operation)?;
            }
            Ok::<_, ToolError>(())
        };
        let ((stdout, overflow), (stderr, _), ()) = tokio::try_join!(
            read_prefix(stdout, stdout_bytes, false),
            read_prefix(stderr, STDERR_BYTES, true),
            feed
        )?;
        let status = super::guard::reap_owned_child(&mut child)
            .await
            .map_err(|_| ToolError::Operation)?;
        Ok::<_, ToolError>(Captured {
            status: status.code(),
            stdout,
            stderr,
            overflow,
        })
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                super::guard::reap_owned_child(&mut child),
            )
            .await;
            Err(ToolError::Timeout(String::new()))
        }
    }
}

pub(super) async fn run(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let git = site.config.git.as_ref().ok_or(ToolError::Configuration)?;
    #[cfg(target_os = "linux")]
    let scratch = Path::new("/scratch");
    #[cfg(not(target_os = "linux"))]
    let scratch = site.root;
    let max_bytes = site.intent.limits.result_bytes as usize;
    let output = if site.intent.call.mutates() {
        write::execute(
            site.workspace,
            scratch,
            git,
            &site.config.git_identity.clone().unwrap_or_default(),
            site.intent,
            site.hidden,
            site.remaining_runtime(),
        )
        .await
    } else {
        execute(
            site.workspace,
            scratch,
            git,
            &site.intent.call,
            site.hidden,
            max_bytes,
            site.remaining_runtime(),
        )
        .await
    };
    #[cfg(target_os = "linux")]
    site.stop_descendants().await?;
    output
}

pub(super) async fn execute(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    call: &ToolCall,
    hidden: &HiddenPaths,
    max_bytes: usize,
    runtime: Duration,
) -> Result<String, ToolError> {
    let deadline = tokio::time::Instant::now() + runtime.min(MAX_GIT_RUNTIME);
    verify_pin(git)?;
    let view = scratch.join(VIEW);
    if let Prepared::Unsupported(message) = prepare_view(workspace, &view)? {
        return Ok(failure(message, &[], max_bytes, hidden));
    }
    let env = environment(&view, workspace, scratch);
    let mut outputs = Vec::new();
    for argv in invocations(call, !hidden.is_empty())? {
        let captured =
            spawn_git(Path::new(&git.executable), &argv, &env, workspace, deadline).await?;
        if !captured.overflow && captured.status != Some(0) {
            let message = captured.status.map_or_else(
                || "git terminated by a signal".to_owned(),
                |code| format!("git exited with status {code}"),
            );
            return Ok(failure(&message, &captured.stderr, max_bytes, hidden));
        }
        outputs.push(captured);
    }
    let budget = max_bytes
        .checked_sub(ENVELOPE_BYTES)
        .ok_or(ToolError::Limit)?;
    let value = match (call, outputs.as_slice()) {
        (ToolCall::GitStatus { .. }, [status]) => {
            status_output(&status.stdout, status.overflow, budget, hidden)?
        }
        (ToolCall::GitDiff { .. }, [numstat, patch]) => {
            diff_output(numstat, patch, budget, hidden)?
        }
        (ToolCall::GitLog { max_count, .. }, [log]) => {
            log_output(&log.stdout, log.overflow, budget, *max_count, hidden)?
        }
        (ToolCall::GitBranch { .. }, [list]) => branch_output(&list.stdout, list.overflow, budget)?,
        _ => return Err(ToolError::Configuration),
    };
    let text = value.to_string();
    if text.len() > max_bytes {
        return Err(ToolError::Limit);
    }
    Ok(text)
}

// Diagnostics cannot be matched against path rules, so a filtered read keeps only its message.
fn failure(message: &str, stderr: &[u8], max_bytes: usize, hidden: &HiddenPaths) -> String {
    let stderr = String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| hidden.is_empty() && !mentions_forbidden(line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut end = stderr
        .len()
        .min(ERROR_BYTES)
        .min(max_bytes.saturating_sub(message.len() + 64));
    while !stderr.is_char_boundary(end) {
        end -= 1;
    }
    let mut map = serde_json::Map::new();
    map.insert("failed".into(), json!(message));
    let detail = stderr[..end].trim_end();
    if !detail.is_empty() {
        map.insert("stderr".into(), json!(detail));
    }
    let text = Value::Object(map).to_string();
    // JSON escaping can still grow control characters; then report the message alone.
    if text.len() <= max_bytes {
        text
    } else {
        json!({"failed": message}).to_string()
    }
}

// Split outside the filename alphabet so punctuation cannot hide a protected path component.
fn mentions_forbidden(line: &str) -> bool {
    line.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    })
    .any(forbidden)
}

struct Rows {
    rows: Vec<Value>,
    used: usize,
    budget: usize,
}

impl Rows {
    fn new(budget: usize) -> Self {
        Self {
            rows: Vec::new(),
            used: 0,
            budget,
        }
    }

    fn push(&mut self, row: Value) -> bool {
        let size = row.to_string().len() + 1;
        if self.rows.len() == MAX_ROWS || self.used + size > self.budget {
            return false;
        }
        self.used += size;
        self.rows.push(row);
        true
    }
}

fn records(raw: &[u8], terminator: u8, overflow: bool) -> Result<Vec<&[u8]>, ToolError> {
    let mut records: Vec<&[u8]> = raw.split(|byte| *byte == terminator).collect();
    let tail = records.pop().unwrap_or_default();
    if !overflow && !tail.is_empty() {
        return Err(ToolError::Operation);
    }
    Ok(records)
}

fn excluded(path: &str) -> bool {
    path.split('/').any(forbidden)
}

fn shown(path: &str, hidden: &HiddenPaths) -> bool {
    !excluded(path) && !hidden.covers(path)
}

fn clipped(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

fn entry(rest: &str, parts: usize) -> Result<(&str, &str), ToolError> {
    let fields: Vec<&str> = rest.splitn(parts, ' ').collect();
    match fields.as_slice() {
        [state, .., path] if fields.len() == parts && state.len() == 2 => Ok((state, path)),
        _ => Err(ToolError::Operation),
    }
}

fn status_output(
    raw: &[u8],
    overflow: bool,
    budget: usize,
    hidden: &HiddenPaths,
) -> Result<Value, ToolError> {
    let mut branch = serde_json::Map::new();
    let mut rows = Rows::new(budget);
    let mut truncated = overflow;
    for record in records(raw, 0, overflow)? {
        let text = String::from_utf8_lossy(record);
        if let Some(header) = text.strip_prefix("# ") {
            if let Some((key, value)) = header
                .strip_prefix("branch.")
                .and_then(|rest| rest.split_once(' '))
            {
                match key {
                    "oid" => {
                        branch.insert(
                            "oid".into(),
                            if value == "(initial)" {
                                Value::Null
                            } else {
                                json!(value)
                            },
                        );
                    }
                    "head" => {
                        branch.insert("head".into(), json!(clipped(value, 200)));
                    }
                    _ => {}
                }
            }
            continue;
        }
        let (state, path) = match text.split_once(' ') {
            Some(("1", rest)) => entry(rest, 8)?,
            Some(("u", rest)) => entry(rest, 10)?,
            Some(("?", path)) => ("??", path),
            _ => return Err(ToolError::Operation),
        };
        if !shown(path, hidden) {
            continue;
        }
        if !rows.push(json!({"path": path, "state": state})) {
            truncated = true;
            break;
        }
    }
    Ok(json!({"branch": branch, "entries": rows.rows, "truncated": truncated}))
}

fn count(value: &str) -> Result<Value, ToolError> {
    if value == "-" {
        return Ok(Value::Null);
    }
    value
        .parse::<u64>()
        .map(Value::from)
        .map_err(|_| ToolError::Operation)
}

fn diff_output(
    numstat: &Captured,
    patch: &Captured,
    budget: usize,
    hidden: &HiddenPaths,
) -> Result<Value, ToolError> {
    let mut rows = Rows::new(budget / 2);
    let mut truncated = numstat.overflow || patch.overflow;
    for record in records(&numstat.stdout, 0, numstat.overflow)? {
        let text = String::from_utf8_lossy(record);
        let mut fields = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(ToolError::Operation);
        };
        if !shown(path, hidden) {
            continue;
        }
        if !rows.push(json!({"path": path, "added": count(added)?, "deleted": count(deleted)?})) {
            truncated = true;
            break;
        }
    }
    let mut remaining = budget - rows.used;
    let mut text = String::new();
    for line in visible_patch(&String::from_utf8_lossy(&patch.stdout), hidden) {
        let size = serde_json::to_string(line)
            .map_err(|_| ToolError::Operation)?
            .len()
            - 2;
        if size > remaining {
            truncated = true;
            break;
        }
        remaining -= size;
        text.push_str(line);
    }
    Ok(json!({"files": rows.rows, "patch": text, "truncated": truncated}))
}

// Every section starts hidden, and only a recognized header naming a shown path reveals it (fail closed).
fn visible_patch<'a>(patch: &'a str, hidden: &'a HiddenPaths) -> impl Iterator<Item = &'a str> {
    let mut hiding = true;
    patch.split_inclusive('\n').filter(move |line| {
        // A capture cut mid-line could shorten a header's path past the filter.
        let Some(text) = line.strip_suffix('\n') else {
            return false;
        };
        if let Some(header) = text.strip_prefix("diff ") {
            hiding = section_path(header).is_none_or(|path| !shown(path, hidden));
        } else if let Some(path) = text.strip_prefix("* Unmerged path ") {
            // A staged diff reports a conflicted path in this one line, without a section header.
            hiding = path.is_empty() || path.starts_with('"') || !shown(path, hidden);
        }
        !hiding
    })
}

// Without renames a two-way header is `a/P b/P`; quoted names stay hidden.
fn section_path(header: &str) -> Option<&str> {
    if let Some(names) = header.strip_prefix("--git ") {
        let path = names.get(2..2 + names.len().checked_sub(5)? / 2)?;
        return (names == format!("a/{path} b/{path}") && !path.starts_with('"')).then_some(path);
    }
    header
        .strip_prefix("--cc ")
        .or_else(|| header.strip_prefix("--combined "))
        .filter(|path| !path.is_empty() && !path.starts_with('"'))
}

fn object_id(bytes: &[u8]) -> Result<&str, ToolError> {
    std::str::from_utf8(bytes)
        .ok()
        .filter(|id| {
            id.len() == 40
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or(ToolError::Operation)
}

// A commit is kept only when one of its `--name-only` lines names a shown path.
fn log_output(
    raw: &[u8],
    overflow: bool,
    budget: usize,
    max_count: u32,
    hidden: &HiddenPaths,
) -> Result<Value, ToolError> {
    let mut commits: Vec<(Value, bool)> = Vec::new();
    for record in records(raw, b'\n', overflow)? {
        if !record.contains(&0) {
            let path = String::from_utf8_lossy(record);
            if let Some((_, evidence)) = commits.last_mut()
                && !path.is_empty()
                && !path.starts_with('"')
                && shown(&path, hidden)
            {
                *evidence = true;
            }
            continue;
        }
        let fields: Vec<&[u8]> = record.split(|byte| *byte == 0).collect();
        let [oid, parents, author, time, subject] = fields.as_slice() else {
            return Err(ToolError::Operation);
        };
        let parents = if parents.is_empty() {
            Vec::new()
        } else {
            parents
                .split(|byte| *byte == b' ')
                .map(object_id)
                .collect::<Result<Vec<_>, _>>()?
        };
        let time = std::str::from_utf8(time)
            .ok()
            .and_then(|time| time.parse::<u64>().ok())
            .ok_or(ToolError::Operation)?;
        let row = json!({
            "oid": object_id(oid)?,
            "parents": parents,
            "author": clipped(&String::from_utf8_lossy(author), 128),
            "time": time,
            "subject": clipped(&String::from_utf8_lossy(subject), 256),
        });
        commits.push((row, hidden.is_empty()));
    }
    let mut rows = Rows::new(budget);
    // An exhausted scan may hide older visible history.
    let mut truncated =
        overflow || !hidden.is_empty() && commits.len() >= MAX_FILTERED_LOG_COMMITS as usize;
    let mut visible = commits.into_iter().filter(|(_, evidence)| *evidence);
    for (row, _) in visible.by_ref().take(max_count as usize) {
        if !rows.push(row) {
            truncated = true;
            break;
        }
    }
    truncated |= visible.next().is_some();
    Ok(json!({"commits": rows.rows, "truncated": truncated}))
}

fn branch_output(raw: &[u8], overflow: bool, budget: usize) -> Result<Value, ToolError> {
    let mut rows = Rows::new(budget);
    let mut truncated = overflow;
    for record in records(raw, b'\n', overflow)? {
        let fields: Vec<&[u8]> = record.split(|byte| *byte == 0).collect();
        let [name, oid, head] = fields.as_slice() else {
            return Err(ToolError::Operation);
        };
        let name = String::from_utf8_lossy(name);
        let row = json!({
            "name": clipped(name.strip_prefix("refs/heads/").ok_or(ToolError::Operation)?, 200),
            "oid": object_id(oid)?,
            "current": *head == b"*",
        });
        if !rows.push(row) {
            truncated = true;
            break;
        }
    }
    Ok(json!({"branches": rows.rows, "truncated": truncated}))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::test_support::process::BoundedOutput;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn none() -> HiddenPaths {
        HiddenPaths::default()
    }

    pub(super) fn host_git_output(
        repository: &Path,
        home: &Path,
        args: &[&str],
    ) -> std::process::Output {
        std::process::Command::new(discover().expect("pinned git").executable)
            // `git commit` otherwise detaches `maintenance run --auto`, which can write `.git` after the fixture returns.
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Arany Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Arany Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .bounded_output()
            .expect("pinned git")
    }

    pub(super) fn host_git(repository: &Path, home: &Path, args: &[&str]) {
        let output = host_git_output(repository, home, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(super) fn fixture(temp: &Path) -> PathBuf {
        let workspace = temp.join("workspace");
        let home = temp.join("home");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(workspace.join("cfg/.ssh")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        host_git(&workspace, &home, &["init", "-q", "-b", "main"]);
        std::fs::write(workspace.join("src/a.txt"), "alpha\n").unwrap();
        std::fs::write(workspace.join("notes.md"), "notes\n").unwrap();
        std::fs::write(workspace.join(".env"), "SECRET_CANARY=1\n").unwrap();
        std::fs::write(workspace.join(".env.local"), "SECRET_CANARY=4\n").unwrap();
        // A case variant in its own directory: beside `.env` it would alias it on case-insensitive APFS.
        std::fs::write(workspace.join("cfg/.Env"), "SECRET_CANARY=6\n").unwrap();
        std::fs::write(workspace.join("cfg/.ssh/id"), "SECRET_CANARY key\n").unwrap();
        let big: String = (0..40_000).map(|n| format!("line-{n:05}\n")).collect();
        std::fs::write(workspace.join("src/big.txt"), big).unwrap();
        host_git(&workspace, &home, &["add", "-A"]);
        host_git(&workspace, &home, &["commit", "-q", "-m", "first commit"]);
        std::fs::write(workspace.join("src/a.txt"), "bravo\n").unwrap();
        std::fs::write(workspace.join(".env"), "SECRET_CANARY=2\n").unwrap();
        host_git(&workspace, &home, &["commit", "-q", "-am", "second commit"]);
        std::fs::write(workspace.join("src/a.txt"), "delta\n").unwrap();
        std::fs::write(workspace.join(".env"), "SECRET_CANARY=3\n").unwrap();
        std::fs::write(workspace.join(".env.local"), "SECRET_CANARY=5\n").unwrap();
        std::fs::write(workspace.join("cfg/.Env"), "SECRET_CANARY=7\n").unwrap();
        let rewritten: String = (0..40_000).map(|n| format!("LINE-{n:05}\n")).collect();
        std::fs::write(workspace.join("src/big.txt"), rewritten).unwrap();
        std::fs::write(workspace.join("src/new.txt"), "new\n").unwrap();
        workspace
    }

    pub(super) fn arm_hostile(temp: &Path, workspace: &Path) -> PathBuf {
        let markers = temp.join("markers");
        let scripts = temp.join("scripts");
        let hooks = temp.join("hooks");
        for dir in [&markers, &scripts, &hooks] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let script = |path: PathBuf, body: &str| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n/usr/bin/touch {}/{name}\n{body}\n",
                    markers.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        script(scripts.join("fsmonitor"), "");
        script(scripts.join("include-fsmonitor"), "");
        script(scripts.join("textconv"), "exec /usr/bin/cat \"$1\"");
        script(scripts.join("clean"), "exec /usr/bin/cat");
        script(scripts.join("external"), "exit 0");
        // pager, gpg and credential have no plain-Git control; they are defense in depth only.
        script(scripts.join("pager"), "exec /usr/bin/cat");
        script(scripts.join("gpg"), "exit 1");
        script(scripts.join("credential"), "exit 0");
        // Reached only with core.hooksPath unset; the config below shadows it, and the control proves the hooksPath route.
        script(hooks.join("post-index-change"), "");
        let git_hook = workspace.join(".git/hooks/post-index-change");
        std::fs::write(
            &git_hook,
            format!(
                "#!/bin/sh\n/usr/bin/touch {}/git-dir-hook\n",
                markers.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git_hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let included = temp.join("included.config");
        std::fs::write(
            &included,
            format!(
                "[core]\n\tfsmonitor = {}\n",
                scripts.join("include-fsmonitor").display()
            ),
        )
        .unwrap();
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap();
        use std::io::Write;
        write!(
            config,
            "[core]\n\tfsmonitor = {fsmonitor}\n\thooksPath = {hooks}\n\tpager = {pager}\n[diff]\n\texternal = {external}\n[diff \"evil\"]\n\ttextconv = {textconv}\n[filter \"evil\"]\n\tclean = {clean}\n[log]\n\tshowSignature = true\n[gpg]\n\tprogram = {gpg}\n[credential]\n\thelper = {credential}\n[include]\n\tpath = {included}\n",
            fsmonitor = scripts.join("fsmonitor").display(),
            hooks = hooks.display(),
            pager = scripts.join("pager").display(),
            external = scripts.join("external").display(),
            textconv = scripts.join("textconv").display(),
            clean = scripts.join("clean").display(),
            gpg = scripts.join("gpg").display(),
            credential = scripts.join("credential").display(),
            included = included.display(),
        )
        .unwrap();
        std::fs::write(
            workspace.join(".gitattributes"),
            "*.txt filter=evil diff=evil\n",
        )
        .unwrap();
        markers
    }

    pub(super) fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &Path, path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                let metadata = std::fs::symlink_metadata(&entry).unwrap();
                let relative = entry.strip_prefix(root).unwrap().to_owned();
                if metadata.file_type().is_symlink() {
                    let target = std::fs::read_link(&entry).unwrap();
                    out.push((relative, target.to_string_lossy().into_owned().into_bytes()));
                } else if metadata.is_dir() {
                    out.push((relative, Vec::new()));
                    walk(root, &entry, out);
                } else {
                    out.push((relative, std::fs::read(&entry).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    pub(super) fn assert_unchanged(root: &Path, before: &[(PathBuf, Vec<u8>)]) {
        let after = tree(root);
        if after == before {
            return;
        }
        let find = |set: &[(PathBuf, Vec<u8>)], path: &Path| {
            set.iter().find(|e| e.0 == path).map(|e| e.1.len())
        };
        let mut diff = Vec::new();
        for (path, bytes) in before {
            match after.iter().find(|e| &e.0 == path) {
                None => diff.push(format!("removed {}", path.display())),
                Some(e) if &e.1 != bytes => diff.push(format!(
                    "changed {} ({:?} -> {:?} bytes)",
                    path.display(),
                    find(before, path),
                    find(&after, path)
                )),
                _ => {}
            }
        }
        for (path, _) in &after {
            if !before.iter().any(|e| &e.0 == path) {
                diff.push(format!("added {}", path.display()));
            }
        }
        panic!("repository .git changed: {diff:#?}");
    }

    pub(super) fn scratch(temp: &Path, index: usize) -> PathBuf {
        let path = temp.join(format!("scratch-{index}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub(super) fn run_call(workspace: &Path, scratch: &Path, call: &ToolCall) -> Value {
        run_filtered(workspace, scratch, call, &none())
    }

    fn run_filtered(
        workspace: &Path,
        scratch: &Path,
        call: &ToolCall,
        hidden: &HiddenPaths,
    ) -> Value {
        let git = discover().expect("pinned system git");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(execute(
                workspace,
                scratch,
                &git,
                call,
                hidden,
                16 * 1024,
                Duration::from_secs(30),
            ))
            .expect("typed Git read");
        serde_json::from_str(&output).unwrap()
    }

    pub(super) struct RestoreMode(pub(super) PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn discover_pins_the_root_owned_system_git() {
        let git = discover().expect("pinned system git");
        assert_eq!(git.name, "git");
        assert!(git_root(Path::new(&git.executable)));
        assert_eq!(
            git.sha256,
            hex_digest(&std::fs::read(&git.executable).unwrap())
        );
        assert!(!git.interpreter && git.inputs.is_empty());
        verify_host_executable(&git).unwrap();
        let mut wrong = git.clone();
        wrong.sha256 = "0".repeat(64);
        assert!(matches!(
            verify_host_executable(&wrong),
            Err(ToolError::ChangedInput)
        ));
    }

    #[test]
    fn executable_magic_matches_the_platform_format() {
        let git = discover().expect("pinned system git");
        assert!(executable_bytes(&git.executable).is_ok());
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("git");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        assert!(matches!(
            executable_bytes(script.to_str().unwrap()),
            Err(ToolError::ChangedInput)
        ));
    }

    #[test]
    fn argv_is_fixed_and_separates_options_revisions_and_pathspecs() {
        assert_eq!(pathspecs("src").len(), 17);
        assert_eq!(pathspecs(".").len(), 16);
        assert_eq!(pathspecs("src")[0], ":(literal)src");
        assert_eq!(pathspecs(""), pathspecs("."));

        let with = |base: &[&str], path: &str| {
            let mut argv = owned(base);
            argv.extend(pathspecs(path));
            argv
        };
        assert_eq!(
            invocations(&ToolCall::GitStatus { path: "src".into() }, false).unwrap(),
            [with(
                &[
                    "--no-pager",
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    "--untracked-files=normal",
                    "--ignore-submodules=all",
                    "--no-renames",
                    "--",
                ],
                "src"
            )]
        );
        let diff = invocations(
            &ToolCall::GitDiff {
                path: "src".into(),
                base: Some("main~1".into()),
                head: Some("HEAD".into()),
                staged: false,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            diff,
            [
                with(
                    &[
                        "--no-pager",
                        "--no-optional-locks",
                        "diff",
                        "--numstat",
                        "-z",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        "--ignore-submodules=all",
                        "--end-of-options",
                        "main~1",
                        "HEAD",
                        "--",
                    ],
                    "src"
                ),
                with(
                    &[
                        "--no-pager",
                        "--no-optional-locks",
                        "diff",
                        "--patch",
                        "--unified=3",
                        "--submodule=short",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        "--ignore-submodules=all",
                        "--end-of-options",
                        "main~1",
                        "HEAD",
                        "--",
                    ],
                    "src"
                ),
            ]
        );
        let cached = invocations(
            &ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: true,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            cached[0],
            with(
                &[
                    "--no-pager",
                    "--no-optional-locks",
                    "diff",
                    "--numstat",
                    "-z",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--ignore-submodules=all",
                    "--cached",
                    "--end-of-options",
                    "--",
                ],
                "."
            )
        );
        assert_eq!(
            invocations(
                &ToolCall::GitLog {
                    path: "src".into(),
                    revision: Some("v1.2".into()),
                    max_count: 5,
                },
                false
            )
            .unwrap(),
            [with(
                &[
                    "--no-pager",
                    "--no-optional-locks",
                    "log",
                    "--no-color",
                    "--no-show-signature",
                    "--max-count=5",
                    "--format=tformat:%H%x00%P%x00%an%x00%at%x00%s",
                    "--end-of-options",
                    "v1.2",
                    "--",
                ],
                "src"
            )]
        );
        assert!(matches!(
            invocations(&ToolCall::List { path: "src".into() }, false),
            Err(ToolError::Configuration)
        ));
    }

    #[test]
    fn environment_is_a_closed_allowlist() {
        let env = environment(
            Path::new("/s/git-view"),
            Path::new("/workspace"),
            Path::new("/scratch"),
        );
        let keys: Vec<&str> = env.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            keys,
            [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "HOME",
                "PATH",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_CONFIG_GLOBAL",
                "GIT_ATTR_NOSYSTEM",
                "GIT_OPTIONAL_LOCKS",
                "GIT_NO_REPLACE_OBJECTS",
                "GIT_NO_LAZY_FETCH",
                "GIT_TERMINAL_PROMPT",
                "LC_ALL",
                "TZ",
            ]
        );
        let value = |key: &str| env.iter().find(|(name, _)| *name == key).unwrap().1.clone();
        assert_eq!(value("GIT_DIR"), OsString::from("/s/git-view"));
        assert_eq!(value("GIT_CONFIG_GLOBAL"), OsString::from("/dev/null"));
        assert_eq!(value("GIT_NO_REPLACE_OBJECTS"), OsString::from("1"));
        assert_eq!(value("PATH"), OsString::from("/usr/bin:/bin"));
    }

    fn captured(stdout: &[u8], overflow: bool) -> Captured {
        Captured {
            status: Some(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            overflow,
        }
    }

    #[test]
    fn status_parser_reads_porcelain_v2_records() {
        let oid = "a".repeat(40);
        let blob = "b".repeat(40);
        let unmerged = "c".repeat(40);
        let raw = format!(
            "# branch.oid {oid}\0# branch.head main\0# stash 1\0\
             1 .M N... 100644 100644 100644 {blob} {blob} src/a b.rs\0\
             u UU N... 100644 100644 100644 100644 {unmerged} {unmerged} {unmerged} c.rs\0\
             ? new.txt\0? .env\0? cfg/.ssh/\0"
        );
        let value = status_output(raw.as_bytes(), false, 4096, &none()).unwrap();
        assert_eq!(value["branch"], json!({"oid": oid, "head": "main"}));
        assert_eq!(
            value["entries"],
            json!([
                {"path": "src/a b.rs", "state": ".M"},
                {"path": "c.rs", "state": "UU"},
                {"path": "new.txt", "state": "??"},
            ])
        );
        assert_eq!(value["truncated"], false);

        let initial = status_output(b"# branch.oid (initial)\0", false, 4096, &none()).unwrap();
        assert_eq!(initial["branch"]["oid"], Value::Null);

        assert!(status_output(b"1 M\0", false, 4096, &none()).is_err());
        assert!(status_output(b"x y\0", false, 4096, &none()).is_err());
        assert!(status_output(b"? a\0? b", false, 4096, &none()).is_err());
        let cut = status_output(b"? a\0? b", true, 4096, &none()).unwrap();
        assert_eq!(cut["entries"], json!([{"path": "a", "state": "??"}]));
        assert_eq!(cut["truncated"], true);
    }

    #[test]
    fn numstat_and_log_parsers_read_nul_fields() {
        let numstat = captured(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0", false);
        let value = diff_output(&numstat, &captured(b"", false), 4096, &none()).unwrap();
        assert_eq!(
            value["files"],
            json!([
                {"path": "src/a.rs", "added": 3, "deleted": 1},
                {"path": "img.png", "added": null, "deleted": null},
            ])
        );
        assert_eq!(value["patch"], "");
        assert_eq!(value["truncated"], false);

        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let log = format!("{a}\0{b}\0Ana\x001700000000\0Fix é\n{b}\0\0Ana\x001699999999\0Root\n");
        let value = log_output(log.as_bytes(), false, 4096, 50, &none()).unwrap();
        let commits = value["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["subject"], "Fix é");
        assert_eq!(commits[1]["parents"], json!([]));
        assert!(log_output(b"a\0b\0Ana\x001700000000\0", false, 4096, 50, &none()).is_err());
        assert!(
            log_output(
                format!("{}\0\0Ana\x001\0s\n", "a".repeat(39)).as_bytes(),
                false,
                4096,
                50,
                &none()
            )
            .is_err()
        );
    }

    #[test]
    fn results_fit_the_budget_and_report_truncation() {
        let many: String = (0..1000).map(|n| format!("? file-{n}.txt\0")).collect();
        let value = status_output(many.as_bytes(), false, 2048, &none()).unwrap();
        assert!(value.to_string().len() <= 2048 + ENVELOPE_BYTES);
        assert_eq!(value["truncated"], true);
        assert!(value["entries"].as_array().unwrap().len() < 1000);

        let capped: String = (0..300).map(|n| format!("? f{n}\0")).collect();
        let value = status_output(capped.as_bytes(), false, 1 << 30, &none()).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(value["truncated"], true);

        let patch = captured(
            &format!("diff --git a/x b/x\n{}", "+x\n".repeat(10_000)).into_bytes(),
            false,
        );
        let value = diff_output(&captured(b"", false), &patch, 4096, &none()).unwrap();
        assert!(value["patch"].as_str().unwrap().ends_with('\n'));
        assert!(value.to_string().len() <= 4096 + ENVELOPE_BYTES);
        assert_eq!(value["truncated"], true);

        assert!(failure("m", &[b'x'; 5000], 200, &none()).len() <= 200);
        assert_eq!(
            failure("m", b"", 4096, &none()),
            json!({"failed": "m"}).to_string()
        );
        let filtered = failure(
            "m",
            b"fatal: cannot read cfg/.ssh/id\nwarning: other\n",
            4096,
            &none(),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&filtered).unwrap()["stderr"],
            "warning: other"
        );
    }

    #[test]
    fn view_contains_only_fixed_configuration_and_data_links() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        arm_hostile(temp.path(), &workspace);
        let view = temp.path().join("scratch").join(VIEW);
        std::fs::create_dir_all(view.parent().unwrap()).unwrap();
        assert!(matches!(
            prepare_view(&workspace, &view),
            Ok(Prepared::Ready)
        ));

        let mut names: Vec<String> = std::fs::read_dir(&view)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["HEAD", "config", "index", "objects", "refs"]);
        assert_eq!(std::fs::read(view.join("config")).unwrap(), VIEW_CONFIG);
        assert!(
            std::fs::symlink_metadata(view.join("index"))
                .unwrap()
                .is_file()
        );
        assert_eq!(
            std::fs::read(view.join("index")).unwrap(),
            std::fs::read(workspace.join(".git/index")).unwrap()
        );
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(
            modified(&view.join("index")),
            modified(&workspace.join(".git/index")),
            "a fresh index mtime hides same-size edits from Git's racy-stat check"
        );
        assert_eq!(
            std::fs::read_to_string(view.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            std::fs::read_link(view.join("objects")).unwrap(),
            workspace.join(".git/objects")
        );
    }

    #[test]
    fn oversized_index_is_a_typed_limit_error() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let git = workspace.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir(git.join("refs")).unwrap();
        std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::File::create(git.join("index"))
            .unwrap()
            .set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .unwrap();
        let view = temp.path().join(VIEW);
        assert!(matches!(
            prepare_view(&workspace, &view),
            Err(ToolError::Limit)
        ));
        assert!(!view.exists());
    }

    #[test]
    fn unsupported_git_metadata_is_not_an_enforcement_cause() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        for name in ["HEAD", "config"] {
            for kind in ["regular", "directory", "hardlink", "fifo"] {
                let temp = tempfile::tempdir().unwrap();
                let workspace = temp.path().join("workspace");
                let git = workspace.join(".git");
                std::fs::create_dir_all(git.join("objects")).unwrap();
                std::fs::create_dir(git.join("refs")).unwrap();
                std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
                std::fs::write(git.join("config"), b"[core]\nrepositoryformatversion = 0\n")
                    .unwrap();
                let target = git.join(name);
                match kind {
                    "regular" => {}
                    "directory" => {
                        std::fs::remove_file(&target).unwrap();
                        std::fs::create_dir(&target).unwrap();
                    }
                    "hardlink" => std::fs::hard_link(&target, temp.path().join("alias")).unwrap(),
                    "fifo" => {
                        std::fs::remove_file(&target).unwrap();
                        mkfifo(&target, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
                    }
                    _ => unreachable!(),
                }
                let view = temp.path().join("view");
                let prepared = prepare_view(&workspace, &view);
                if kind == "regular" {
                    assert!(matches!(prepared, Ok(Prepared::Ready)), "{name}/{kind}");
                } else {
                    assert!(
                        matches!(prepared, Ok(Prepared::Unsupported(UNSUPPORTED_LAYOUT))),
                        "{name}/{kind} must retain the unsupported-layout outcome"
                    );
                    assert!(!view.exists());
                }
            }
        }
    }

    #[test]
    fn unsupported_repositories_fail_closed_without_running_git() {
        let check = |name: &str, prepare: &dyn Fn(&Path, &Path), message: &str| {
            let temp = tempfile::tempdir().unwrap();
            let workspace = fixture(temp.path());
            prepare(temp.path(), &workspace);
            let value = run_call(
                &workspace,
                &scratch(temp.path(), 0),
                &ToolCall::GitStatus { path: ".".into() },
            );
            assert_eq!(value, json!({"failed": message}), "{name}");
        };
        check(
            "no .git",
            &|_, workspace| std::fs::remove_dir_all(workspace.join(".git")).unwrap(),
            NO_REPOSITORY,
        );
        check(
            "gitfile",
            &|_, workspace| {
                std::fs::remove_dir_all(workspace.join(".git")).unwrap();
                std::fs::write(workspace.join(".git"), "gitdir: /elsewhere\n").unwrap();
            },
            UNSUPPORTED_LAYOUT,
        );
        check(
            "symlinked .git",
            &|temp, workspace| {
                let other = fixture(&temp.join("other"));
                std::fs::remove_dir_all(workspace.join(".git")).unwrap();
                std::os::unix::fs::symlink(other.join(".git"), workspace.join(".git")).unwrap();
            },
            UNSUPPORTED_LAYOUT,
        );
        check(
            "extensions",
            &|_, workspace| {
                use std::io::Write;
                let mut config = std::fs::OpenOptions::new()
                    .append(true)
                    .open(workspace.join(".git/config"))
                    .unwrap();
                config
                    .write_all(b"[extensions]\n\tobjectFormat = sha256\n")
                    .unwrap();
            },
            UNSUPPORTED_FORMAT,
        );
        check(
            "sha256 HEAD",
            &|_, workspace| {
                std::fs::write(workspace.join(".git/HEAD"), format!("{}\n", "a".repeat(64)))
                    .unwrap()
            },
            UNSUPPORTED_FORMAT,
        );
        check(
            "symlinked objects",
            &|_, workspace| {
                std::fs::rename(
                    workspace.join(".git/objects"),
                    workspace.join(".git/objects-real"),
                )
                .unwrap();
                std::os::unix::fs::symlink(
                    workspace.join(".git/objects-real"),
                    workspace.join(".git/objects"),
                )
                .unwrap();
            },
            UNSUPPORTED_LAYOUT,
        );
        check(
            "missing refs",
            &|_, workspace| std::fs::remove_dir_all(workspace.join(".git/refs")).unwrap(),
            UNSUPPORTED_LAYOUT,
        );
        check(
            "hard-linked config",
            &|temp, workspace| {
                std::fs::rename(workspace.join(".git/config"), temp.join("config-copy")).unwrap();
                std::fs::hard_link(temp.join("config-copy"), workspace.join(".git/config"))
                    .unwrap();
            },
            UNSUPPORTED_LAYOUT,
        );

        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        host_git(&workspace, &home, &["update-index", "--split-index"]);
        let markers = arm_hostile(temp.path(), &workspace);
        let value = run_call(
            &workspace,
            &scratch(temp.path(), 1),
            &ToolCall::GitStatus { path: ".".into() },
        );
        assert_eq!(value["failed"], "git exited with status 128");
        assert_eq!(std::fs::read_dir(markers).unwrap().count(), 0);
    }

    #[test]
    fn hostile_fixture_executes_under_plain_git() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let markers = arm_hostile(temp.path(), &workspace);
        let home = temp.path().join("home");
        // Plain Git may fail after running hostile code (the required smudge filter); only execution matters.
        for args in [&["status"][..], &["diff"], &["diff", "--no-ext-diff"]] {
            host_git_output(&workspace, &home, args);
        }
        // An intent-to-add entry writes the index, which fires post-index-change through core.hooksPath.
        host_git_output(&workspace, &home, &["add", "-N", "src/new.txt"]);
        for name in [
            "include-fsmonitor",
            "external",
            "textconv",
            "clean",
            "post-index-change",
        ] {
            assert!(
                markers.join(name).exists(),
                "{name} did not execute under plain Git"
            );
        }
    }

    #[test]
    fn hostile_repository_configuration_never_executes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let markers = arm_hostile(temp.path(), &workspace);
        let before = tree(&workspace.join(".git"));
        // Unit runs are not sandboxed; the chmod stands in for the Guard's read-only bind.
        std::fs::set_permissions(
            workspace.join(".git"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let _restore = RestoreMode(workspace.join(".git"));

        let mut outputs = Vec::new();
        let status = run_call(
            &workspace,
            &scratch(temp.path(), 0),
            &ToolCall::GitStatus { path: ".".into() },
        );
        assert!(status.get("failed").is_none(), "{status}");
        let entries = status["entries"].as_array().unwrap();
        for expected in [
            json!({"path": "src/a.txt", "state": ".M"}),
            json!({"path": "src/big.txt", "state": ".M"}),
            json!({"path": "src/new.txt", "state": "??"}),
            json!({"path": ".gitattributes", "state": "??"}),
        ] {
            assert!(entries.contains(&expected), "{expected}");
        }
        assert_eq!(status["branch"]["head"], "main");
        outputs.push(status.to_string());

        let diff = run_call(
            &workspace,
            &scratch(temp.path(), 1),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            },
        );
        assert!(diff.get("failed").is_none(), "{diff}");
        let files = diff["files"].as_array().unwrap();
        assert!(files.contains(&json!({"path": "src/a.txt", "added": 1, "deleted": 1})));
        assert!(files.contains(&json!({"path": "src/big.txt", "added": 40000, "deleted": 40000})));
        let patch = diff["patch"].as_str().unwrap();
        assert!(patch.contains("-bravo") && patch.contains("+delta"));
        assert_eq!(diff["truncated"], true);
        assert!(diff.to_string().len() <= 16 * 1024);
        outputs.push(diff.to_string());

        let cached = run_call(
            &workspace,
            &scratch(temp.path(), 2),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: true,
            },
        );
        assert_eq!(cached["files"], json!([]));
        assert_eq!(cached["patch"], "");
        outputs.push(cached.to_string());

        let range = run_call(
            &workspace,
            &scratch(temp.path(), 3),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: Some("HEAD~1".into()),
                head: Some("HEAD".into()),
                staged: false,
            },
        );
        assert_eq!(
            range["files"],
            json!([{"path": "src/a.txt", "added": 1, "deleted": 1}])
        );
        let patch = range["patch"].as_str().unwrap();
        assert!(patch.contains("-alpha") && patch.contains("+bravo"));
        outputs.push(range.to_string());

        let log = run_call(
            &workspace,
            &scratch(temp.path(), 4),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 10,
            },
        );
        let commits = log["commits"].as_array().unwrap();
        assert_eq!(commits[0]["subject"], "second commit");
        assert_eq!(commits[1]["subject"], "first commit");
        assert_eq!(commits[0]["author"], "Arany Test");
        assert_eq!(commits[0]["time"], 1_700_000_000u64);
        assert_eq!(commits[1]["parents"], json!([]));
        outputs.push(log.to_string());

        let missing = run_call(
            &workspace,
            &scratch(temp.path(), 5),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing-branch".into()),
                max_count: 1,
            },
        );
        assert_eq!(missing["failed"], "git exited with status 128");
        outputs.push(missing.to_string());

        for output in &outputs {
            for canary in [".env", "SECRET_CANARY", ".ssh"] {
                assert!(!output.contains(canary), "{canary} leaked into Tool output");
            }
        }
        assert_eq!(std::fs::read_dir(&markers).unwrap().count(), 0);
        assert_unchanged(&workspace.join(".git"), &before);
    }

    #[test]
    fn stale_stat_status_never_writes_repository_or_view_index() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let markers = arm_hostile(temp.path(), &workspace);
        // A stale stat on an unchanged tracked file makes status want to refresh the index.
        std::fs::File::options()
            .write(true)
            .open(workspace.join("notes.md"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let before = tree(&workspace.join(".git"));
        let view_scratch = scratch(temp.path(), 0);
        let status = run_call(
            &workspace,
            &view_scratch,
            &ToolCall::GitStatus { path: ".".into() },
        );
        assert!(status.get("failed").is_none(), "{status}");
        assert_eq!(
            std::fs::read(view_scratch.join(VIEW).join("index")).unwrap(),
            std::fs::read(workspace.join(".git/index")).unwrap()
        );
        assert_eq!(tree(&workspace.join(".git")), before);
        assert_eq!(std::fs::read_dir(markers).unwrap().count(), 0);
    }

    #[test]
    fn stale_stat_diff_never_writes_repository_index() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let markers = arm_hostile(temp.path(), &workspace);
        std::fs::File::options()
            .write(true)
            .open(workspace.join("notes.md"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let before = tree(&workspace.join(".git"));
        let diff = run_call(
            &workspace,
            &scratch(temp.path(), 0),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            },
        );
        assert!(diff.get("failed").is_none(), "{diff}");
        assert_eq!(
            diff["files"],
            json!([
                {"path": "src/a.txt", "added": 1, "deleted": 1},
                {"path": "src/big.txt", "added": 40000, "deleted": 40000},
            ])
        );
        let patch = diff["patch"].as_str().unwrap();
        assert!(patch.contains("-bravo") && patch.contains("+delta"));
        assert_eq!(tree(&workspace.join(".git")), before);
        assert_eq!(std::fs::read_dir(markers).unwrap().count(), 0);
    }

    #[test]
    fn stderr_lines_naming_secret_components_are_filtered() {
        for line in [
            "fatal: (.env)",
            "error: .env,",
            "option path=.env",
            "fatal: cannot read cfg/.ssh/id",
            "warning: `.env.local`",
            "x\"'.npmrc'",
        ] {
            assert!(mentions_forbidden(line), "{line}");
        }
        assert!(!mentions_forbidden("warning: other"));
        assert!(!mentions_forbidden("fatal: environment.txt"));
    }

    #[test]
    fn secret_components_match_regardless_of_ascii_case() {
        for path in [
            ".ENV",
            ".Env",
            ".SSH/id_rsa",
            ".AWS/credentials",
            ".env.LOCAL",
        ] {
            assert!(excluded(path), "{path}");
            assert!(mentions_forbidden(&format!("fatal: {path}")), "{path}");
        }
        assert!(pathspecs("src").contains(&":(icase,exclude,glob)**/.env".to_owned()));
        assert!(pathspecs("src").contains(&":(icase,exclude,glob)**/.aws/**".to_owned()));
        assert!(pathspecs("src").contains(&":(icase,exclude,glob)**/.env.*".to_owned()));
        assert!(!excluded("src/environment.rs"));
    }

    #[test]
    fn extensions_are_detected_after_a_byte_order_mark() {
        assert!(uses_extensions(
            b"\xef\xbb\xbf[extensions]\n\tobjectFormat = sha256\n"
        ));
        assert!(uses_extensions(b"[Extensions]\n"));
        assert!(!uses_extensions(b"[core]\n\tbare = false\n"));
    }

    #[test]
    fn numstat_rows_and_patch_hunks_drop_secret_paths() {
        let numstat = captured(
            b"1\t1\tsrc/ok.rs\x000\t3\t.env.local\x000\t1\tcfg/.ssh/id\x000\t2\t.ENV\x00",
            false,
        );
        let patch = captured(
            b"diff --git a/.env.local b/.env.local\n+SECRET_CANARY=1\ndiff --git a/.ENV b/.ENV\n+SECRET_CANARY=2\ndiff --git a/src/ok.rs b/src/ok.rs\n+ok\n",
            false,
        );
        let value = diff_output(&numstat, &patch, 4096, &none()).unwrap();
        assert_eq!(
            value["files"],
            json!([{"path": "src/ok.rs", "added": 1, "deleted": 1}])
        );
        let text = value["patch"].as_str().unwrap();
        assert!(text.contains("+ok"));
        assert!(!text.contains("SECRET_CANARY"));
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
            let (name, email) = user_section(text);
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

    #[test]
    fn user_config_fifo_without_a_writer_is_refused_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("config");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(read_user_config(&fifo)).ok());
        let read = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("FIFO open must not block");
        assert!(read.is_none());
    }

    fn private_hidden() -> HiddenPaths {
        crate::tools::permissions::PermissionConfig::parse(
            br#"{"permissions":{"deny":["Read(./private/**)"]}}"#,
        )
        .unwrap()
        .hidden_paths(&ToolCall::GitStatus { path: ".".into() })
    }

    #[test]
    fn restricted_rows_hunks_and_history_are_dropped_before_fitting() {
        let hidden = private_hidden();
        // An overflow made only of hidden rows neither displaces visible rows nor marks truncation.
        let status: String = (0..300)
            .map(|n| format!("? private/f{n}\0"))
            .chain(["? private/\0".to_owned(), "? src/a\0".to_owned()])
            .collect();
        let value = status_output(status.as_bytes(), false, 4096, &hidden).unwrap();
        assert_eq!(value["entries"], json!([{"path": "src/a", "state": "??"}]));
        assert_eq!(value["truncated"], false);

        let numstat = captured(b"1\t0\tprivate/s\x001\t0\tsrc/a\x00", false);
        let patch = [
            "diff --git a/src/a b/src/a\n+ok\n",
            // A conflict's combined section never inherits the previous section's visibility.
            "diff --cc private/c\n++DENY_CANARY\n",
            "diff --git a/private/s b/private/s\n+DENY_CANARY\n",
            "diff --git a/private/x b/y b/private/x b/y\n+DENY_CANARY\n",
            "diff --git \"a/private/caf\\303\\251\" \"b/private/caf\\303\\251\"\n+DENY_CANARY\n",
            "diff --unknown src/a\n+DENY_CANARY\n",
            "diff --cc src/b\n++both\n",
            "* Unmerged path private/u\n",
            "* Unmerged path src/u\n",
            "diff --cc priv",
        ]
        .concat();
        let value =
            diff_output(&numstat, &captured(patch.as_bytes(), false), 4096, &hidden).unwrap();
        assert_eq!(
            value["files"],
            json!([{"path": "src/a", "added": 1, "deleted": 0}])
        );
        assert_eq!(
            value["patch"],
            "diff --git a/src/a b/src/a\n+ok\ndiff --cc src/b\n++both\n* Unmerged path src/u\n"
        );

        let commit = |n: u8, subject: &str, paths: &str| {
            format!(
                "{}\0\0Ana\x001\0{subject}\n\n{paths}",
                char::from(b'a' + n).to_string().repeat(40)
            )
        };
        let log = [
            commit(0, "visible", "src/a\nprivate/s\n"),
            commit(1, "DENY_CANARY", "private/s\n"),
            commit(2, "no evidence", ""),
            commit(3, "later", "src/b\n"),
        ]
        .concat();
        let subjects = |value: Value| -> (Vec<String>, bool) {
            let subjects = value["commits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|commit| commit["subject"].as_str().unwrap().to_owned())
                .collect();
            (subjects, value["truncated"].as_bool().unwrap())
        };
        assert_eq!(
            subjects(log_output(log.as_bytes(), false, 4096, 5, &hidden).unwrap()),
            (vec!["visible".to_owned(), "later".to_owned()], false)
        );
        assert_eq!(
            subjects(log_output(log.as_bytes(), false, 4096, 1, &hidden).unwrap()),
            (vec!["visible".to_owned()], true)
        );
        let exhausted: String = (0..MAX_FILTERED_LOG_COMMITS)
            .map(|_| commit(1, "covered", "private/s\n"))
            .collect();
        assert_eq!(
            subjects(log_output(exhausted.as_bytes(), false, 1 << 30, 5, &hidden).unwrap()),
            (Vec::new(), true),
            "an exhausted scan is incomplete, not empty history"
        );
        assert_eq!(
            failure("m", b"fatal: private/s\n", 4096, &hidden),
            json!({"failed": "m"}).to_string()
        );
    }

    #[test]
    fn conflicted_and_historical_restricted_paths_stay_out_of_git_results() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("private")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let git = |args: &[&str]| host_git(&workspace, &home, args);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(workspace.join("private/c"), "base\n").unwrap();
        std::fs::write(workspace.join("a"), "base\n").unwrap();
        std::fs::write(workspace.join("moved"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "side"]);
        std::fs::write(workspace.join("private/c"), "side DENY_CANARY\n").unwrap();
        git(&["commit", "-q", "-am", "side DENY_CANARY"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(workspace.join("private/c"), "main DENY_CANARY\n").unwrap();
        std::fs::write(workspace.join("a"), "main\n").unwrap();
        git(&["commit", "-q", "-am", "main"]);
        // A rename into a covered directory still deletes a visible path, so the commit stays visible.
        git(&["mv", "moved", "private/moved"]);
        git(&["commit", "-q", "-m", "move into private"]);
        // The merge conflicts in private/c, so `git diff` reports it as a combined section.
        host_git_output(&workspace, &home, &["merge", "-q", "side"]);
        std::fs::write(workspace.join("a"), "edited\n").unwrap();
        let hidden = private_hidden();
        let run = |index: usize, call: ToolCall| {
            let plain = run_call(&workspace, &scratch(temp.path(), 2 * index), &call).to_string();
            let filtered = run_filtered(
                &workspace,
                &scratch(temp.path(), 2 * index + 1),
                &call,
                &hidden,
            )
            .to_string();
            (plain, filtered)
        };
        let (plain, filtered) = run(
            0,
            ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            },
        );
        assert!(
            plain.contains("diff --cc private/c") && plain.contains("DENY_CANARY"),
            "{plain}"
        );
        assert!(
            !filtered.contains("DENY_CANARY") && !filtered.contains("private"),
            "{filtered}"
        );
        assert!(filtered.contains("+edited"), "{filtered}");
        // Git collapses an untracked directory to `private/fresh/`, which the rule still covers.
        std::fs::create_dir_all(workspace.join("private/fresh")).unwrap();
        std::fs::write(workspace.join("private/fresh/f"), "DENY_CANARY\n").unwrap();
        let (plain, filtered) = run(1, ToolCall::GitStatus { path: ".".into() });
        assert!(
            plain.contains("private/c") && plain.contains("private/fresh/"),
            "{plain}"
        );
        assert!(
            !filtered.contains("private") && filtered.contains("\"a\""),
            "{filtered}"
        );
        let (plain, filtered) = run(
            2,
            ToolCall::GitLog {
                path: ".".into(),
                revision: Some("side".into()),
                max_count: 1,
            },
        );
        assert!(plain.contains("DENY_CANARY"), "{plain}");
        let filtered: Value = serde_json::from_str(&filtered).unwrap();
        assert_eq!(filtered["commits"][0]["subject"], "base", "{filtered}");
        assert_eq!(filtered["truncated"], false);
        let (_, filtered) = run(
            3,
            ToolCall::GitLog {
                path: ".".into(),
                revision: Some("main".into()),
                max_count: 1,
            },
        );
        let filtered: Value = serde_json::from_str(&filtered).unwrap();
        assert_eq!(
            filtered["commits"][0]["subject"], "move into private",
            "{filtered}"
        );
        // A staged diff reports the conflicted path only as an `* Unmerged path` line.
        git(&["add", "a"]);
        let (plain, filtered) = run(
            4,
            ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: true,
            },
        );
        assert!(plain.contains("* Unmerged path private/c"), "{plain}");
        assert!(
            !filtered.contains("private") && filtered.contains("+edited"),
            "{filtered}"
        );
    }
}

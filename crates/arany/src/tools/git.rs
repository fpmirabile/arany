pub(crate) mod host;
mod write;

pub(crate) use write::recover_locks;

use super::ToolError;
use super::config::{GitIdentity, Program};
use super::executable::{EXECUTABLES, git_root, has_magic};
use super::guard::GuardContext;
use super::permissions::HiddenPaths;
use super::selection::{CONTROL_BYTES, Selector};
use super::types::{MAX_GIT_LOG_COMMITS, MAX_SNAPSHOT_BYTES, ToolCall};
use crate::validate::{
    FORBIDDEN_NAMES, MAX_REVISION_BYTES, MAX_REVISION_RANGE_BYTES, forbidden, hex_digest,
    valid_relative, valid_revision,
};
use cap_fs_ext::{DirExt, MetadataExt as CapMetadataExt};
use cap_std::fs::Dir;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::future::Future;
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
                && revision.as_deref().is_none_or(valid_log_revision)
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
            name,
            start_point,
            scope,
            base,
            max_count,
        } => match name {
            None => {
                start_point.is_none()
                    && scope
                        .as_deref()
                        .is_none_or(|scope| matches!(scope, "local" | "remote" | "all"))
                    && base.as_deref().is_none_or(valid_revision)
                    && max_count.is_none_or(|count| (1..=MAX_ROWS as u32).contains(&count))
            }
            Some(name) => {
                valid_branch(name)
                    && start_point.as_deref().is_none_or(valid_revision)
                    && scope.is_none()
                    && base.is_none()
                    && max_count.is_none()
            }
        },
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
    let revision = json!({"type":["string","null"], "description":format!("null, or HEAD, a branch or tag name, or a 7-64 character hex commit name, with at most four ~N or ^N ancestry steps (N at most 9999). No ranges, ':', '@{{' or options. At most {MAX_REVISION_BYTES} bytes.")});
    let log_revision = json!({"type":["string","null"], "description":format!("null, one revision, or one base..head range. A range lists commits reachable from head but not base; its result includes the captured endpoint revisions and commit OIDs. Each endpoint is HEAD, a branch or tag name, or a 7-64 character hex commit name, with at most four ~N or ^N ancestry steps (N at most 9999), at most {MAX_REVISION_BYTES} bytes each; the range is at most {MAX_REVISION_RANGE_BYTES} bytes. Ranges have no open endpoints, three-dot form, revision sets, ':', '@{{' or options.")});
    Some(match operation {
        "git_status" => vec![path],
        "git_diff" => vec![
            path,
            ("base", revision.clone()),
            ("head", revision),
            (
                "staged",
                json!({"type":"boolean", "description":"true compares the index with base, or HEAD when base is null; head must then be null. false compares the working tree with the index (no revisions), with base, or compares the two endpoint trees using Git's two-dot semantics, not a merge-base comparison."}),
            ),
        ],
        "git_log" => vec![
            path,
            ("revision", log_revision),
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
                json!({"type":["string","null"], "description":"null lists branch tips; otherwise the new branch name: letters, digits, '.', '_', '-' and '/' separators, at most 128 bytes."}),
            ),
            ("start_point", revision),
            (
                "scope",
                json!({"type":["string","null"], "enum":[null,"local","remote","all"], "description":"Read scope; null means local. Remote means locally stored remote-tracking refs, which may be stale; never fetches. Must be null for creation."}),
            ),
            (
                "base",
                json!({"type":["string","null"], "description":"Optional comparison revision, resolved once to a commit OID. Counts describe whole available history: behind is base-only commits, ahead is tip-only commits. Counts are withheld beyond 1000 commits or one second per row. Must be null for creation."}),
            ),
            (
                "max_count",
                json!({"type":["integer","null"], "description":"Returned ref rows: 1 through 256; null means 256. Symbolic remote aliases name their target and have no independent counts. Must be null for creation."}),
            ),
        ],
        "git_switch" => vec![(
            "branch",
            json!({"type":"string", "description":"Existing local branch name."}),
        )],
        _ => return host::schema(operation),
    })
}

const GIT_CAPTURE_BYTES: usize = 256 * 1024;
const STDERR_BYTES: usize = 4 * 1024;
const ERROR_BYTES: usize = 1024;
const MAX_ROWS: usize = 256;
const MAX_SUBJECT_CHARS: usize = 256;
const MAX_FILTERED_LOG_COMMITS: u32 = 1000;
const MAX_BRANCH_COUNT_COMMITS: usize = 1000;
pub(super) const DISCLOSURE_CONTRACT: &[u8] = b";typed-git-ignore-disclosure-v1:status-diff-log-paths-and-patches-filtered-before-bounds;tracked-ignored-paths-hidden;log-subjects-require-visible-path-evidence;path-read-stderr-suppressed;replay-never-reruns-selection";
// Semantic unit fixtures allow slow native spawns; production retains its row deadline.
const MAX_BRANCH_ROW_RUNTIME: Duration = if cfg!(test) {
    Duration::from_secs(10)
} else {
    Duration::from_secs(1)
};
const BRANCH_CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const ENVELOPE_BYTES: usize = 1024;
const HEAD_BYTES: usize = 256;
const REF_BYTES: usize = 1024;
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

enum LogRevision<'a> {
    Single,
    Range { base: &'a str, head: &'a str },
}

fn log_revision(revision: &str) -> Option<LogRevision<'_>> {
    if let Some((base, head)) = revision.split_once("..") {
        (revision.len() <= MAX_REVISION_RANGE_BYTES
            && !head.contains("..")
            && valid_revision(base)
            && valid_revision(head))
        .then_some(LogRevision::Range { base, head })
    } else {
        valid_revision(revision).then_some(LogRevision::Single)
    }
}

fn valid_log_revision(revision: &str) -> bool {
    log_revision(revision).is_some()
}

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
    Ready(Repository),
    Unsupported(&'static str),
}

struct Repository {
    git_dir: Dir,
    identity: (u64, u64),
    head: Vec<u8>,
    index: Option<(Vec<u8>, std::time::SystemTime)>,
    links: Vec<&'static str>,
    config: Vec<u8>,
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
    let config = match read_optional(&git_dir, "config", CONFIG_BYTES) {
        Ok(Some(config)) if uses_extensions(&config) => {
            return Ok(Err(UNSUPPORTED_FORMAT));
        }
        Ok(config) => config.unwrap_or_default(),
        Err(ToolError::Limit | ToolError::Path) => {
            return Ok(Err(UNSUPPORTED_LAYOUT));
        }
        Err(error) => return Err(error),
    };
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
        config,
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
    Ok(Prepared::Ready(repository))
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

// A branch's object name, from its loose file or else `packed-refs`, never following links.
fn resolve(git_dir: &Dir, name: &str) -> Result<Option<String>, ToolError> {
    if let Some(oid) = loose_ref(git_dir, name)? {
        return Ok(Some(oid));
    }
    let Some(packed) = read_optional(git_dir, "packed-refs", MAX_SNAPSHOT_BYTES)? else {
        return Ok(None);
    };
    for line in packed.split(|byte| *byte == b'\n') {
        if let Some((oid, reference)) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.split_once(' '))
            && reference == name
        {
            return object_id(oid.as_bytes()).map(|oid| Some(oid.to_owned()));
        }
    }
    Ok(None)
}

fn loose_ref(git_dir: &Dir, name: &str) -> Result<Option<String>, ToolError> {
    let (parent, leaf) = name.rsplit_once('/').ok_or(ToolError::Operation)?;
    let mut directory = git_dir.try_clone().map_err(|_| ToolError::Operation)?;
    let mut loose = true;
    for part in parent.split('/') {
        match directory.open_dir_nofollow(part) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                loose = false;
                break;
            }
            Err(_) => return Err(ToolError::Path),
        }
    }
    if loose && let Some(bytes) = read_optional(&directory, leaf, REF_BYTES)? {
        let oid = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        return object_id(oid).map(|oid| Some(oid.to_owned()));
    }
    Ok(None)
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
        ToolCall::GitBranch {
            name: None,
            scope,
            max_count,
            ..
        } => {
            let mut argv = owned(&[
                "--no-pager",
                "--no-optional-locks",
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname)%00%(objectname)%00%(HEAD)%00%(symref)%00%(objecttype)",
            ]);
            argv.push(format!(
                "--count={}",
                max_count.unwrap_or(MAX_ROWS as u32) + 1
            ));
            match scope.as_deref().unwrap_or("local") {
                "local" => argv.push("refs/heads/".into()),
                "remote" => argv.push("refs/remotes/".into()),
                "all" => argv.extend(owned(&["refs/heads/", "refs/remotes/"])),
                _ => return Err(ToolError::Configuration),
            }
            runs.push(argv);
        }
        _ => return Err(ToolError::Configuration),
    }
    Ok(runs)
}

fn comparison_revisions(call: &ToolCall) -> Option<(&str, &str)> {
    match call {
        ToolCall::GitDiff {
            base: Some(base),
            head: Some(head),
            ..
        } => Some((base, head)),
        ToolCall::GitLog {
            revision: Some(revision),
            ..
        } => match log_revision(revision)? {
            LogRevision::Single => None,
            LogRevision::Range { base, head } => Some((base, head)),
        },
        _ => None,
    }
}

async fn resolve_revision(
    git: &Path,
    env: &[(&'static str, OsString)],
    workspace: &Path,
    revision: &str,
    deadline: tokio::time::Instant,
) -> Result<Captured, ToolError> {
    let expression = format!("{revision}^{{commit}}");
    spawn_git(
        git,
        &owned(&[
            "--no-pager",
            "--no-optional-locks",
            "rev-parse",
            "--verify",
            "--end-of-options",
            &expression,
        ]),
        env,
        workspace,
        deadline,
    )
    .await
}

struct Comparison {
    base_revision: String,
    head_revision: String,
    base_oid: String,
    head_oid: String,
}

enum ComparisonResolution {
    NotApplicable,
    Resolved(Comparison),
    Failed(String),
}

enum CapturedOutputs {
    Ready(Vec<Captured>),
    Failed(String),
}

enum DisclosedCapture {
    Ready {
        outputs: Vec<Captured>,
        disclosure: GitDisclosure,
    },
    Failed(String),
}

async fn capture_with_disclosure(
    capture: impl Future<Output = Result<CapturedOutputs, ToolError>>,
    workspace: &Path,
    hidden: &HiddenPaths,
    disclose_paths: bool,
    action_deadline: tokio::time::Instant,
) -> Result<DisclosedCapture, ToolError> {
    match capture.await? {
        CapturedOutputs::Failed(output) => Ok(DisclosedCapture::Failed(output)),
        CapturedOutputs::Ready(outputs) => Ok(DisclosedCapture::Ready {
            outputs,
            disclosure: if disclose_paths {
                GitDisclosure::new(workspace, hidden, action_deadline)?
            } else {
                GitDisclosure::empty(hidden.clone())
            },
        }),
    }
}

struct ComparisonContext<'a> {
    git: &'a Path,
    env: &'a [(&'static str, OsString)],
    workspace: &'a Path,
    deadline: tokio::time::Instant,
    max_bytes: usize,
    hidden: &'a HiddenPaths,
}

impl ComparisonContext<'_> {
    async fn resolve(&self, call: &ToolCall) -> Result<ComparisonResolution, ToolError> {
        let Some((base, head)) = comparison_revisions(call) else {
            return Ok(ComparisonResolution::NotApplicable);
        };
        let mut oids = Vec::with_capacity(2);
        for revision in [base, head] {
            let capture =
                resolve_revision(self.git, self.env, self.workspace, revision, self.deadline)
                    .await?;
            if capture.overflow {
                return Ok(ComparisonResolution::Failed(failure(
                    "git revision output exceeded its capture bound",
                    failure_stderr(call, &capture.stderr),
                    self.max_bytes,
                    self.hidden,
                )));
            }
            if let Some(failed) = capture_failure(call, &capture, self.max_bytes, self.hidden) {
                return Ok(ComparisonResolution::Failed(failed));
            }
            if !capture.stderr.is_empty() {
                return Ok(ComparisonResolution::Failed(failure(
                    "git revision resolution was ambiguous or unsupported",
                    b"",
                    self.max_bytes,
                    self.hidden,
                )));
            }
            let bytes = capture
                .stdout
                .strip_suffix(b"\n")
                .unwrap_or(&capture.stdout);
            oids.push(object_id(bytes)?.to_owned());
        }
        Ok(ComparisonResolution::Resolved(Comparison {
            base_revision: base.to_owned(),
            head_revision: head.to_owned(),
            base_oid: oids[0].clone(),
            head_oid: oids[1].clone(),
        }))
    }
}

struct PreparedGitRead<'a> {
    workspace: &'a Path,
    git: &'a Program,
    call: &'a ToolCall,
    hidden: &'a HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
    env: Vec<(&'static str, OsString)>,
}

impl PreparedGitRead<'_> {
    async fn run(self, repository: &Repository) -> Result<String, ToolError> {
        let budget = self
            .max_bytes
            .checked_sub(ENVELOPE_BYTES)
            .ok_or(ToolError::Limit)?;
        let comparison = match (ComparisonContext {
            git: Path::new(&self.git.executable),
            env: &self.env,
            workspace: self.workspace,
            deadline: self.deadline,
            max_bytes: self.max_bytes,
            hidden: self.hidden,
        })
        .resolve(self.call)
        .await?
        {
            ComparisonResolution::NotApplicable => None,
            ComparisonResolution::Resolved(comparison) => Some(comparison),
            ComparisonResolution::Failed(output) => return Ok(output),
        };
        let call = comparison.as_ref().map_or_else(
            || self.call.clone(),
            |comparison| with_comparison_oids(self.call, comparison),
        );
        let captured = capture_with_disclosure(
            self.capture_outputs(&call, repository),
            self.workspace,
            self.hidden,
            path_git_read(&call),
            self.deadline,
        )
        .await?;
        let (outputs, mut disclosure) = match captured {
            DisclosedCapture::Ready {
                outputs,
                disclosure,
            } => (outputs, disclosure),
            DisclosedCapture::Failed(output) => return Ok(output),
        };
        let mut value = self
            .render(&call, &outputs, repository, budget, &mut disclosure)
            .await?;
        if let Some(comparison) = &comparison
            && let Value::Object(fields) = &mut value
        {
            fields.insert("comparison".into(), comparison.output());
        }
        let text = value.to_string();
        if text.len() > self.max_bytes {
            return Err(ToolError::Limit);
        }
        Ok(text)
    }

    async fn capture_outputs(
        &self,
        call: &ToolCall,
        repository: &Repository,
    ) -> Result<CapturedOutputs, ToolError> {
        let branch_read = matches!(call, ToolCall::GitBranch { name: None, .. });
        let mut outputs = Vec::new();
        let filtered = !self.hidden.is_empty() || matches!(call, ToolCall::GitLog { .. });
        for argv in invocations(call, filtered)? {
            let capture = match spawn_git(
                Path::new(&self.git.executable),
                &argv,
                &self.env,
                self.workspace,
                self.deadline,
            )
            .await
            {
                Err(ToolError::Timeout(_)) if branch_read => {
                    return Ok(CapturedOutputs::Failed(
                        branch_result(repository, Vec::new(), None, true, true).to_string(),
                    ));
                }
                result => result?,
            };
            if let Some(failed) = capture_failure(call, &capture, self.max_bytes, self.hidden) {
                return Ok(CapturedOutputs::Failed(failed));
            }
            outputs.push(capture);
        }
        Ok(CapturedOutputs::Ready(outputs))
    }

    async fn render(
        &self,
        call: &ToolCall,
        outputs: &[Captured],
        repository: &Repository,
        budget: usize,
        disclosure: &mut GitDisclosure,
    ) -> Result<Value, ToolError> {
        match (call, outputs) {
            (ToolCall::GitStatus { .. }, [status]) => {
                status_output(&status.stdout, status.overflow, budget, disclosure)
            }
            (ToolCall::GitDiff { .. }, [numstat, patch]) => {
                diff_output(numstat, patch, budget, disclosure)
            }
            (ToolCall::GitLog { max_count, .. }, [log]) => log_output(
                &log.stdout,
                log.overflow,
                budget,
                LogLimit::typed(*max_count),
                disclosure,
            ),
            (
                ToolCall::GitBranch {
                    base, max_count, ..
                },
                [list],
            ) => {
                BranchRead {
                    git: Path::new(&self.git.executable),
                    env: &self.env,
                    workspace: self.workspace,
                    deadline: self.deadline,
                    hidden: self.hidden,
                    repository,
                    budget,
                }
                .output(list, max_count.unwrap_or(MAX_ROWS as u32), base.as_deref())
                .await
            }
            _ => Err(ToolError::Configuration),
        }
    }
}

impl Comparison {
    fn output(&self) -> Value {
        json!({
            "base": {"revision": self.base_revision, "oid": self.base_oid},
            "head": {"revision": self.head_revision, "oid": self.head_oid},
        })
    }
}

fn with_comparison_oids(call: &ToolCall, comparison: &Comparison) -> ToolCall {
    let mut call = call.clone();
    match &mut call {
        ToolCall::GitDiff { base, head, .. } => {
            *base = Some(comparison.base_oid.clone());
            *head = Some(comparison.head_oid.clone());
        }
        ToolCall::GitLog { revision, .. } => {
            *revision = Some(format!("{}..{}", comparison.base_oid, comparison.head_oid));
        }
        _ => {}
    }
    call
}

fn capture_failure(
    call: &ToolCall,
    capture: &Captured,
    max_bytes: usize,
    hidden: &HiddenPaths,
) -> Option<String> {
    if !capture.overflow && capture.status != Some(0) {
        let message = capture.status.map_or_else(
            || "git terminated by a signal".to_owned(),
            |code| format!("git exited with status {code}"),
        );
        Some(failure(
            &message,
            failure_stderr(call, &capture.stderr),
            max_bytes,
            hidden,
        ))
    } else {
        None
    }
}

fn failure_stderr<'a>(call: &ToolCall, stderr: &'a [u8]) -> &'a [u8] {
    if path_git_read(call) { &[] } else { stderr }
}

fn path_git_read(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } | ToolCall::GitLog { .. }
    )
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
            stdout_bytes: GIT_CAPTURE_BYTES,
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
    // The executor unit's Seatbelt profile is already installed and every child inherits it.
    #[cfg(target_os = "macos")]
    {
        let _ = writable;
        super::guard::spawn_before(command, deadline)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
            Err(timeout_after_reap(super::guard::reap_owned_child(&mut child)).await)
        }
    }
}

async fn timeout_after_reap(
    reap: impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
) -> ToolError {
    match tokio::time::timeout(Duration::from_secs(1), reap).await {
        Ok(Ok(_)) => ToolError::Timeout(String::new()),
        Ok(Err(_)) | Err(_) => ToolError::Operation,
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
        #[cfg(target_os = "macos")]
        {
            write_stage(site, git).await
        }
        #[cfg(not(target_os = "macos"))]
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
        execute(GitReadRequest {
            workspace: site.workspace,
            scratch,
            git,
            call: &site.intent.call,
            hidden: site.hidden,
            max_bytes,
            runtime: site.remaining_runtime(),
        })
        .await
    };
    #[cfg(target_os = "linux")]
    site.stop_descendants().await?;
    output
}

pub(super) const WRITE_FRAME_BYTES: usize = write::FRAME_BYTES;

#[cfg(target_os = "macos")]
async fn write_stage(site: &GuardContext<'_>, git: &Program) -> Result<String, ToolError> {
    use super::guard::GitStage;
    match site.git.stage {
        Some(GitStage::Plan) => {
            let identity = site.config.git_identity.clone().unwrap_or_default();
            match write::plan_frame(
                site.workspace,
                site.root,
                git,
                &identity,
                site.intent,
                site.hidden,
                site.remaining_runtime(),
            )
            .await?
            {
                Ok((frame, result)) => {
                    *site.git_plan.lock().map_err(|_| ToolError::Operation)? = Some(frame);
                    Ok(result.to_string())
                }
                Err(failure) => Ok(failure),
            }
        }
        Some(GitStage::Publish) => {
            let (result, frame) = site.git.input.as_ref().ok_or(ToolError::Configuration)?;
            let result: serde_json::Value =
                serde_json::from_slice(result).map_err(|_| ToolError::Configuration)?;
            if !result.is_object() || result.get("publication").is_some() {
                return Err(ToolError::Configuration);
            }
            write::publish_frame(site.workspace, frame, result, site.intent, site.hidden)
        }
        None => Err(ToolError::Configuration),
    }
}

pub(super) struct GitReadRequest<'a> {
    pub(super) workspace: &'a Path,
    pub(super) scratch: &'a Path,
    pub(super) git: &'a Program,
    pub(super) call: &'a ToolCall,
    pub(super) hidden: &'a HiddenPaths,
    pub(super) max_bytes: usize,
    pub(super) runtime: Duration,
}

pub(super) async fn execute(request: GitReadRequest<'_>) -> Result<String, ToolError> {
    let GitReadRequest {
        workspace,
        scratch,
        git,
        call,
        hidden,
        max_bytes,
        runtime,
    } = request;
    let branch_read = matches!(call, ToolCall::GitBranch { name: None, .. });
    let runtime = runtime.min(MAX_GIT_RUNTIME);
    let work_runtime = if branch_read {
        runtime.saturating_sub(BRANCH_CLEANUP_RESERVE)
    } else {
        runtime
    };
    let deadline = tokio::time::Instant::now() + work_runtime;
    verify_pin(git)?;
    let view = scratch.join(VIEW);
    let repository = match prepare_view(workspace, &view)? {
        Prepared::Ready(repository) => repository,
        Prepared::Unsupported(message) => return Ok(failure(message, &[], max_bytes, hidden)),
    };
    let env = environment(&view, workspace, scratch);
    PreparedGitRead {
        workspace,
        git,
        call,
        hidden,
        max_bytes,
        deadline,
        env,
    }
    .run(&repository)
    .await
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

struct GitDisclosure {
    hidden: HiddenPaths,
    root: Option<Dir>,
    selector: Selector,
    controls: BTreeSet<String>,
}

impl GitDisclosure {
    fn new(
        workspace: &Path,
        hidden: &HiddenPaths,
        action_deadline: tokio::time::Instant,
    ) -> Result<Self, ToolError> {
        let mut disclosure = Self {
            hidden: hidden.clone(),
            root: Some(super::fs::open_directory(workspace)?),
            selector: Selector::discovery_until(action_deadline),
            controls: BTreeSet::new(),
        };
        disclosure.control("")?;
        Ok(disclosure)
    }

    fn empty(hidden: HiddenPaths) -> Self {
        Self {
            hidden,
            root: None,
            selector: Selector::discovery_until(
                tokio::time::Instant::now() + super::selection::DISCOVERY_TIME,
            ),
            controls: BTreeSet::new(),
        }
    }

    fn shown(&mut self, path: &str, directory: bool) -> Result<bool, ToolError> {
        let path = path.strip_suffix('/').unwrap_or(path);
        if excluded(path) || self.hidden.covers(path) {
            return Ok(false);
        }
        let Some(_) = self.root else {
            return Ok(true);
        };
        if !valid_relative(path, false) || path.split('/').count() > super::glob::MAX_DEPTH {
            return Ok(false);
        }
        for (index, _) in path.match_indices('/') {
            let ancestor = &path[..index];
            if self.hidden.covers(ancestor) || self.selector.excluded(ancestor, true)? {
                return Ok(false);
            }
            self.control(ancestor)?;
        }
        Ok(!self.selector.excluded(path, directory)?)
    }

    fn control(&mut self, directory: &str) -> Result<(), ToolError> {
        let control = if directory.is_empty() {
            ".gitignore".to_owned()
        } else {
            format!("{directory}/.gitignore")
        };
        if self.controls.contains(&control) {
            return Ok(());
        }
        if control.split('/').count() > super::glob::MAX_DEPTH || self.hidden.covers(&control) {
            return Err(ToolError::FilesystemDenied);
        }
        let Some(root) = &self.root else {
            return Ok(());
        };
        let parent = if directory.is_empty() {
            root.try_clone().map_err(|_| ToolError::Path)?
        } else {
            match super::fs::open_file(root, directory) {
                Ok(file) => {
                    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
                    if !metadata.is_dir() {
                        self.controls.insert(control);
                        return Ok(());
                    }
                    Dir::from_std_file(file)
                }
                Err(ToolError::NotFound) => {
                    self.controls.insert(control);
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        };
        let before = match parent.symlink_metadata(".gitignore") {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.controls.insert(control);
                return Ok(());
            }
            Err(_) => return Err(ToolError::Path),
            Ok(metadata) if !metadata.is_file() || metadata.nlink() != 1 => {
                return Err(ToolError::FilesystemDenied);
            }
            Ok(metadata) => metadata,
        };
        let file = super::fs::open_file(root, &control)?;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || (metadata.dev(), metadata.ino()) != (before.dev(), before.ino())
        {
            return Err(ToolError::FilesystemDenied);
        }
        let bytes = super::fs::read_regular(file, CONTROL_BYTES)?;
        self.selector.add(directory, &bytes)?;
        self.controls.insert(control);
        Ok(())
    }
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
    disclosure: &mut GitDisclosure,
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
        if !disclosure.shown(path, path.ends_with('/'))? {
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
    disclosure: &mut GitDisclosure,
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
        if !disclosure.shown(path, false)? {
            continue;
        }
        if !rows.push(json!({"path": path, "added": count(added)?, "deleted": count(deleted)?})) {
            truncated = true;
            break;
        }
    }
    let mut remaining = budget - rows.used;
    let mut text = String::new();
    for line in visible_patch(&String::from_utf8_lossy(&patch.stdout), disclosure)? {
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
fn visible_patch<'a>(
    patch: &'a str,
    disclosure: &mut GitDisclosure,
) -> Result<Vec<&'a str>, ToolError> {
    let mut hiding = true;
    let mut visible = Vec::new();
    for line in patch.split_inclusive('\n') {
        // A capture cut mid-line could shorten a header's path past the filter.
        let Some(text) = line.strip_suffix('\n') else {
            continue;
        };
        if let Some(header) = text.strip_prefix("diff ") {
            hiding = match section_path(header) {
                Some(path) => !disclosure.shown(path, false)?,
                None => true,
            };
        } else if let Some(path) = text.strip_prefix("* Unmerged path ") {
            // A staged diff reports a conflicted path in this one line, without a section header.
            hiding = path.is_empty() || path.starts_with('"') || !disclosure.shown(path, false)?;
        }
        if !hiding {
            visible.push(line);
        }
    }
    Ok(visible)
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

struct LogLimit {
    max_count: u32,
    require_visible_path: bool,
}

impl LogLimit {
    fn typed(max_count: u32) -> Self {
        Self {
            max_count,
            require_visible_path: true,
        }
    }

    fn branch_tip() -> Self {
        Self {
            max_count: 1,
            require_visible_path: false,
        }
    }
}

// A commit is kept only when one of its `--name-only` lines names a shown path.
fn log_output(
    raw: &[u8],
    overflow: bool,
    budget: usize,
    limit: LogLimit,
    disclosure: &mut GitDisclosure,
) -> Result<Value, ToolError> {
    let mut commits: Vec<(Value, bool)> = Vec::new();
    for record in records(raw, b'\n', overflow)? {
        if !record.contains(&0) {
            let path = String::from_utf8_lossy(record);
            if let Some((_, evidence)) = commits.last_mut()
                && !path.is_empty()
                && !path.starts_with('"')
                && disclosure.shown(&path, false)?
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
            "subject": clipped(&String::from_utf8_lossy(subject), MAX_SUBJECT_CHARS),
        });
        commits.push((row, !limit.require_visible_path));
    }
    let mut rows = Rows::new(budget);
    // An exhausted scan may hide older visible history.
    let mut truncated = overflow
        || limit.require_visible_path && commits.len() >= MAX_FILTERED_LOG_COMMITS as usize;
    let mut visible = commits.into_iter().filter(|(_, evidence)| *evidence);
    for (row, _) in visible.by_ref().take(limit.max_count as usize) {
        if !rows.push(row) {
            truncated = true;
            break;
        }
    }
    truncated |= visible.next().is_some();
    Ok(json!({"commits": rows.rows, "truncated": truncated}))
}

struct BranchRead<'a> {
    git: &'a Path,
    env: &'a [(&'static str, OsString)],
    workspace: &'a Path,
    deadline: tokio::time::Instant,
    hidden: &'a HiddenPaths,
    repository: &'a Repository,
    budget: usize,
}

enum BranchCompletion {
    Finished { truncated: bool },
    TimedOut,
    Failed(Value),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum UpstreamStatus {
    Present,
    Gone,
    Unavailable,
    Unsupported,
    NotApplicable,
}

type PackedUpstreams = BTreeMap<String, UpstreamStatus>;

impl BranchRead<'_> {
    fn packed_upstreams(
        &self,
        config: Option<&UpstreamConfig>,
        listed: &[&[u8]],
    ) -> Result<PackedUpstreams, ToolError> {
        let mut targets: PackedUpstreams = listed
            .iter()
            .filter_map(|record| {
                let name = std::str::from_utf8(record.split(|byte| *byte == 0).next()?).ok()?;
                let branch = name.strip_prefix("refs/heads/")?;
                let target = config?.upstream(branch).ok()??;
                Some((target, UpstreamStatus::Gone))
            })
            .collect();
        if targets.is_empty() {
            return Ok(targets);
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(ToolError::Timeout(String::new()));
        }
        let packed =
            match read_optional(&self.repository.git_dir, "packed-refs", MAX_SNAPSHOT_BYTES) {
                Ok(packed) => packed.unwrap_or_default(),
                Err(_) => {
                    targets
                        .values_mut()
                        .for_each(|status| *status = UpstreamStatus::Unsupported);
                    return Ok(targets);
                }
            };
        for line in packed.split(|byte| *byte == b'\n') {
            if tokio::time::Instant::now() >= self.deadline {
                return Err(ToolError::Timeout(String::new()));
            }
            if let Some((oid, name)) = std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.split_once(' '))
                && let Some(status) = targets.get_mut(name)
                && *status == UpstreamStatus::Gone
            {
                *status = if object_id(oid.as_bytes()).is_ok() {
                    UpstreamStatus::Present
                } else {
                    UpstreamStatus::Unsupported
                };
            }
        }
        Ok(targets)
    }

    async fn capture(
        &self,
        argv: &[String],
        deadline: tokio::time::Instant,
    ) -> Result<Captured, ToolError> {
        spawn_git(self.git, argv, self.env, self.workspace, deadline).await
    }

    fn failed(&self, capture: &Captured) -> Option<Value> {
        (capture.status != Some(0) || capture.overflow).then(|| {
            let message = if capture.overflow {
                "git branch output exceeded its capture bound".to_owned()
            } else {
                capture.status.map_or_else(
                    || "git terminated by a signal".to_owned(),
                    |code| format!("git exited with status {code}"),
                )
            };
            let text = failure(&message, &capture.stderr, self.budget, self.hidden);
            serde_json::from_str(&text).expect("serialized failure")
        })
    }

    async fn output(
        &self,
        list: &Captured,
        max_count: u32,
        base: Option<&str>,
    ) -> Result<Value, ToolError> {
        let comparison = if let Some(base) = base {
            let capture = match self
                .capture(
                    &owned(&[
                        "--no-pager",
                        "--no-optional-locks",
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{base}^{{commit}}"),
                    ]),
                    self.deadline,
                )
                .await
            {
                Err(ToolError::Timeout(_)) => {
                    return Ok(branch_result(self.repository, Vec::new(), None, true, true));
                }
                result => result?,
            };
            if let Some(failed) = self.failed(&capture) {
                return Ok(failed);
            }
            let oid = object_id(
                capture
                    .stdout
                    .strip_suffix(b"\n")
                    .unwrap_or(&capture.stdout),
            )?;
            Some(json!({"revision": base, "oid": oid}))
        } else {
            None
        };
        let mut rows = Rows::new(self.budget);
        match self
            .collect(list, max_count, comparison.as_ref(), &mut rows)
            .await?
        {
            BranchCompletion::Finished { truncated } => Ok(branch_result(
                self.repository,
                rows.rows,
                comparison,
                truncated,
                false,
            )),
            BranchCompletion::TimedOut => Ok(branch_result(
                self.repository,
                rows.rows,
                comparison,
                true,
                true,
            )),
            BranchCompletion::Failed(failed) => Ok(failed),
        }
    }

    async fn collect(
        &self,
        list: &Captured,
        max_count: u32,
        comparison: Option<&Value>,
        rows: &mut Rows,
    ) -> Result<BranchCompletion, ToolError> {
        if tokio::time::Instant::now() >= self.deadline {
            return Ok(BranchCompletion::TimedOut);
        }
        let upstreams = upstream_config(&self.repository.config);
        let listed = records(&list.stdout, b'\n', list.overflow)?;
        let mut truncated = list.overflow || listed.len() > max_count as usize;
        let packed = match self.packed_upstreams(
            upstreams.as_ref(),
            &listed[..listed.len().min(max_count as usize)],
        ) {
            Err(ToolError::Timeout(_)) => return Ok(BranchCompletion::TimedOut),
            result => result?,
        };
        for record in listed.into_iter().take(max_count as usize) {
            if tokio::time::Instant::now() >= self.deadline {
                return Ok(BranchCompletion::TimedOut);
            }
            let mut row = branch_row(
                record,
                upstreams.as_ref(),
                &self.repository.git_dir,
                &packed,
                comparison.is_some(),
            )?;
            truncated |= row["name_truncated"] == true || row["symbolic_target_truncated"] == true;
            let enrich = self.hidden.is_empty()
                && row["symbolic_target"].is_null()
                && row["kind"] != "non-commit";
            // Reserve escaped subject/count space so enrichment is only done for returned rows.
            let enrichment_bytes = if enrich {
                MAX_SUBJECT_CHARS * 6 + 2 * 20
            } else {
                0
            };
            if rows.used + row.to_string().len() + 1 + enrichment_bytes > rows.budget {
                truncated = true;
                break;
            }
            if enrich {
                match self.enrich(&mut row, comparison, &mut truncated).await {
                    Ok(Some(failed)) => return Ok(BranchCompletion::Failed(failed)),
                    Ok(None) => {}
                    Err(ToolError::Timeout(_)) => {
                        row["counts_capped"] = json!(comparison.is_some());
                        truncated = true;
                    }
                    Err(error) => return Err(error),
                }
                truncated |= row["counts_capped"] == true;
            }
            if !rows.push(row) {
                truncated = true;
                break;
            }
            if tokio::time::Instant::now() >= self.deadline {
                return Ok(BranchCompletion::TimedOut);
            }
        }
        Ok(BranchCompletion::Finished { truncated })
    }

    async fn enrich(
        &self,
        row: &mut Value,
        comparison: Option<&Value>,
        truncated: &mut bool,
    ) -> Result<Option<Value>, ToolError> {
        let deadline = self
            .deadline
            .min(tokio::time::Instant::now() + MAX_BRANCH_ROW_RUNTIME);
        let oid = row["oid"].as_str().ok_or(ToolError::Operation)?.to_owned();
        let capture = self
            .capture(
                &owned(&[
                    "--no-pager",
                    "--no-optional-locks",
                    "log",
                    "-1",
                    "--no-color",
                    "--no-show-signature",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--name-only",
                    "--no-renames",
                    "--diff-merges=first-parent",
                    "--format=tformat:%H%x00%P%x00%an%x00%at%x00%s",
                    "--end-of-options",
                    &oid,
                    "--",
                ]),
                deadline,
            )
            .await?;
        *truncated |= capture.overflow;
        if !capture.overflow
            && let Some(failed) = self.failed(&capture)
        {
            return Ok(Some(failed));
        }
        let subject = tip_subject(
            &capture.stdout,
            capture.overflow,
            &oid,
            self.hidden,
            !self.repository.links.contains(&"shallow"),
        )?;
        row["subject_withheld"] = json!(subject.is_none());
        row["subject"] = json!(subject);
        drop(capture);
        if let Some(comparison) = comparison {
            let base_oid = comparison["oid"].as_str().ok_or(ToolError::Operation)?;
            let capture = self
                .capture(
                    &owned(&[
                        "--no-pager",
                        "--no-optional-locks",
                        "rev-list",
                        "--left-right",
                        &format!("--max-count={}", MAX_BRANCH_COUNT_COMMITS + 1),
                        "--end-of-options",
                        &format!("{base_oid}...{oid}"),
                        "--",
                    ]),
                    deadline,
                )
                .await?;
            if let Some(failed) = self.failed(&capture) {
                return Ok(Some(failed));
            }
            match branch_counts(&capture.stdout)? {
                Some((behind, ahead)) => {
                    row["behind"] = json!(behind);
                    row["ahead"] = json!(ahead);
                    row["counts_withheld"] = json!(false);
                }
                None => row["counts_capped"] = json!(true),
            }
        }
        Ok(None)
    }
}

fn branch_result(
    repository: &Repository,
    branches: Vec<Value>,
    comparison_base: Option<Value>,
    truncated: bool,
    timed_out: bool,
) -> Value {
    json!({"branches": branches, "comparison_base": comparison_base,
        "history_complete": !repository.links.contains(&"shallow"), "truncated": truncated, "timed_out": timed_out})
}

fn branch_row(
    raw: &[u8],
    config: Option<&UpstreamConfig>,
    git_dir: &Dir,
    packed: &PackedUpstreams,
    comparison: bool,
) -> Result<Value, ToolError> {
    let fields: Vec<_> = raw.split(|byte| *byte == 0).collect();
    let [name, oid, head, symbolic, kind] = fields.as_slice() else {
        return Err(ToolError::Operation);
    };
    let name_exact = std::str::from_utf8(name).is_ok();
    let name = String::from_utf8_lossy(name);
    let name = name.as_ref();
    let local = name.starts_with("refs/heads/");
    if !local && !name.starts_with("refs/remotes/") {
        return Err(ToolError::Operation);
    }
    let symbolic_exact = std::str::from_utf8(symbolic).is_ok();
    let symbolic_text = String::from_utf8_lossy(symbolic);
    let symbolic = (!symbolic.is_empty()).then_some(symbolic_text.as_ref());
    let (upstream, status) = if local && !name_exact {
        (None, UpstreamStatus::Unsupported)
    } else {
        upstream_status(config, git_dir, packed, name)
    };
    let commit = *kind == b"commit";
    let mut row = json!({"name": bounded_ref_name(name), "name_truncated": !name_exact || name.len() > REF_BYTES,
        "kind": if !commit { "non-commit" } else if local { "local" } else { "remote" },
        "oid": object_id(oid)?, "current": *head == b"*", "symbolic_target": symbolic.map(bounded_ref_name),
        "subject": null, "subject_withheld": symbolic.is_none() || !commit,
        "upstream": upstream, "upstream_status": status, "ahead": null, "behind": null,
        "counts_withheld": !commit || symbolic.is_none() && comparison, "counts_capped": false});
    if !symbolic_exact || symbolic.is_some_and(|name| name.len() > REF_BYTES) {
        row["symbolic_target_truncated"] = json!(true);
    }
    Ok(row)
}

fn bounded_ref_name(name: &str) -> &str {
    let mut end = name.len().min(REF_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn branch_counts(raw: &[u8]) -> Result<Option<(u64, u64)>, ToolError> {
    let mut behind = 0;
    let mut ahead = 0;
    let records = records(raw, b'\n', false)?;
    if records.len() > MAX_BRANCH_COUNT_COMMITS {
        return Ok(None);
    }
    for record in records {
        let (side, oid) = record.split_first().ok_or(ToolError::Operation)?;
        object_id(oid)?;
        match side {
            b'<' => behind += 1,
            b'>' => ahead += 1,
            _ => return Err(ToolError::Operation),
        }
    }
    Ok(Some((behind, ahead)))
}

fn tip_subject(
    raw: &[u8],
    overflow: bool,
    oid: &str,
    hidden: &HiddenPaths,
    history_complete: bool,
) -> Result<Option<String>, ToolError> {
    let visible = records(raw, b'\n', overflow)?
        .into_iter()
        .filter(|record| !record.contains(&0))
        .any(|path| {
            std::str::from_utf8(path)
                .is_ok_and(|path| !path.is_empty() && !path.starts_with('"') && shown(path, hidden))
        });
    let log = log_output(
        raw,
        overflow,
        GIT_CAPTURE_BYTES,
        LogLimit::branch_tip(),
        &mut GitDisclosure::empty(hidden.clone()),
    )?;
    let tip = log["commits"]
        .as_array()
        .and_then(|rows| rows.first())
        .filter(|tip| tip["oid"] == oid);
    let Some(tip) = tip else {
        return if overflow {
            Ok(None)
        } else {
            Err(ToolError::Operation)
        };
    };
    let visible = visible
        && (history_complete
            || tip["parents"]
                .as_array()
                .is_some_and(|parents| !parents.is_empty()));
    let subject = tip["subject"].as_str().ok_or(ToolError::Operation)?;
    Ok(visible.then(|| subject.to_owned()))
}

fn upstream_status(
    config: Option<&UpstreamConfig>,
    git_dir: &Dir,
    packed: &PackedUpstreams,
    name: &str,
) -> (Option<String>, UpstreamStatus) {
    let Some(branch) = name.strip_prefix("refs/heads/") else {
        return (None, UpstreamStatus::NotApplicable);
    };
    match config.map(|config| config.upstream(branch)) {
        Some(Ok(Some(target))) => match loose_ref(git_dir, &target) {
            Ok(Some(_)) => (Some(target), UpstreamStatus::Present),
            Ok(None) => match packed
                .get(&target)
                .copied()
                .unwrap_or(UpstreamStatus::Unsupported)
            {
                status @ (UpstreamStatus::Present | UpstreamStatus::Gone) => (Some(target), status),
                UpstreamStatus::Unavailable
                | UpstreamStatus::Unsupported
                | UpstreamStatus::NotApplicable => (None, UpstreamStatus::Unsupported),
            },
            Err(_) => (None, UpstreamStatus::Unsupported),
        },
        Some(Ok(None)) => (None, UpstreamStatus::Unavailable),
        Some(Err(())) | None => (None, UpstreamStatus::Unsupported),
    }
}

struct UpstreamConfig {
    values: std::collections::BTreeMap<(String, String, String), Vec<String>>,
}

// Configuration is descriptive data only; Git always receives VIEW_CONFIG.
fn upstream_config(bytes: &[u8]) -> Option<UpstreamConfig> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut values = std::collections::BTreeMap::<_, Vec<String>>::new();
    let mut section = (String::new(), String::new());
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.contains('\\') || line.matches('"').count() % 2 != 0 {
            return None;
        }
        if line.starts_with('[') {
            let header = line.strip_prefix('[')?.strip_suffix(']')?.trim();
            let (kind, name) = header
                .split_once(char::is_whitespace)
                .unwrap_or((header, ""));
            let kind = kind.to_ascii_lowercase();
            if matches!(kind.split('.').next(), Some("include" | "includeif"))
                || kind.starts_with("branch.")
                || kind.starts_with("remote.")
            {
                return None;
            }
            let name = name.trim();
            let name = if name.is_empty() {
                ""
            } else {
                name.strip_prefix('"')?.strip_suffix('"')?
            };
            if name.contains(['\\', '"']) {
                return None;
            }
            section = (kind, name.to_owned());
            continue;
        }
        if !matches!(section.0.as_str(), "branch" | "remote") {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim().to_ascii_lowercase();
        if !matches!(
            (section.0.as_str(), key.as_str()),
            ("branch", "remote" | "merge") | ("remote", "fetch")
        ) {
            continue;
        }
        let value = value.trim();
        let value = if value.starts_with('"') {
            value.strip_prefix('"')?.strip_suffix('"')?
        } else {
            value
        };
        if value.contains(['\\', '"']) {
            return None;
        }
        values
            .entry((section.0.clone(), section.1.clone(), key))
            .or_default()
            .push(value.to_owned());
    }
    Some(UpstreamConfig { values })
}

impl UpstreamConfig {
    fn upstream(&self, branch: &str) -> Result<Option<String>, ()> {
        let get = |section: &str, name: &str, key: &str| {
            self.values.get(&(section.into(), name.into(), key.into()))
        };
        let remote = get("branch", branch, "remote");
        let merge = get("branch", branch, "merge");
        if remote.is_some_and(|values| values.len() != 1)
            || merge.is_some_and(|values| values.len() != 1)
        {
            return Err(());
        }
        if remote.is_none() || merge.is_none() {
            return Ok(None);
        }
        let ([remote], [merge]) = (remote.ok_or(())?.as_slice(), merge.ok_or(())?.as_slice())
        else {
            return Err(());
        };
        if !merge.starts_with("refs/heads/") || !valid_branch(merge) {
            return Err(());
        }
        if remote == "." {
            return Ok(Some(merge.clone()));
        }
        if !valid_branch(remote) {
            return Err(());
        }
        let Some(mappings) = get("remote", remote, "fetch") else {
            return Ok(None);
        };
        let [mapping] = mappings.as_slice() else {
            return Err(());
        };
        let mapping = mapping.strip_prefix('+').unwrap_or(mapping);
        let (source, target) = mapping.split_once(':').ok_or(())?;
        if !source.starts_with("refs/heads/")
            || !(target.starts_with("refs/heads/") || target.starts_with("refs/remotes/"))
        {
            return Err(());
        }
        let target = match (source.split_once('*'), target.split_once('*')) {
            (None, None) if valid_branch(source) && valid_branch(target) && source == merge => {
                target.to_owned()
            }
            (Some((prefix, suffix)), Some((before, after)))
                if !suffix.contains('*')
                    && !after.contains('*')
                    && valid_branch(&source.replace('*', "branch"))
                    && valid_branch(&target.replace('*', "branch")) =>
            {
                let middle = merge
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                    .ok_or(())?;
                format!("{before}{middle}{after}")
            }
            _ => return Err(()),
        };
        if !valid_branch(&target) {
            return Err(());
        }
        Ok(Some(target))
    }
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
            .block_on(execute(GitReadRequest {
                workspace,
                scratch,
                git: &git,
                call,
                hidden,
                max_bytes: 16 * 1024,
                runtime: Duration::from_secs(30),
            }))
            .expect("typed Git read");
        serde_json::from_str(&output).unwrap()
    }

    pub(super) struct RestoreMode(pub(super) PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    fn branches(scope: Option<&str>, base: Option<&str>, max_count: Option<u32>) -> ToolCall {
        ToolCall::GitBranch {
            name: None,
            start_point: None,
            scope: scope.map(str::to_owned),
            base: base.map(str::to_owned),
            max_count,
        }
    }

    #[test]
    fn branch_discovery_pins_divergence_tracking_aliases_and_disclosure() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        let oid = |args: &[&str]| {
            let output = host_git_output(&workspace, &home, args);
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let base = oid(&["rev-parse", "HEAD"]);
        let tree_oid = oid(&["rev-parse", "HEAD^{tree}"]);
        let parent = oid(&["rev-parse", "HEAD~1"]);
        let divergent = oid(&[
            "commit-tree",
            &tree_oid,
            "-p",
            &parent,
            "-m",
            "divergent tip",
        ]);
        for index in 0..8 {
            host_git(
                &workspace,
                &home,
                &[
                    "update-ref",
                    &format!("refs/heads/branch-{index:02}"),
                    &divergent,
                ],
            );
        }
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/heads/origin/topic", &base],
        );
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/remotes/origin/topic", &divergent],
        );
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/remotes/review/topic", &base],
        );
        host_git(
            &workspace,
            &home,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/topic",
            ],
        );
        host_git(&workspace, &home, &["pack-refs", "--all"]);
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/heads/branch-08", &divergent],
        );
        use std::io::Write;
        let config = b"\n[remote \"review\"]\nfetch = +refs/heads/*:refs/remotes/review/*\n[branch \"branch-00\"]\nremote = \"review\"\nmerge = \"refs/heads/topic\"\n[branch \"branch-01\"]\nremote = .\nmerge = refs/heads/main\n[branch \"branch-02\"]\nremote = review\nmerge = refs/heads/gone\n";
        std::fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap()
            .write_all(config)
            .unwrap();
        let before = tree(&workspace.join(".git"));
        let call = branches(Some("all"), Some("main"), None);
        let unrestricted =
            crate::tools::permissions::PermissionConfig::parse(br#"{"permissions":{}}"#)
                .unwrap()
                .hidden_paths(&call);
        assert!(unrestricted.is_empty());
        let result = run_filtered(&workspace, &scratch(temp.path(), 500), &call, &unrestricted);
        assert_eq!(
            result["comparison_base"],
            json!({"revision":"main", "oid":base})
        );
        assert_eq!(result["history_complete"], true);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["timed_out"], false);
        let rows = result["branches"].as_array().unwrap();
        assert_eq!(rows.len(), 14);
        for (index, row) in rows.iter().take(9).enumerate() {
            assert_eq!(row["name"], format!("refs/heads/branch-{index:02}"));
            assert_eq!(row["oid"], divergent);
            assert_eq!(
                (row["behind"].as_u64(), row["ahead"].as_u64()),
                (Some(1), Some(1))
            );
            assert_eq!(row["subject"], "divergent tip");
        }
        assert_eq!(rows[0]["upstream"], "refs/remotes/review/topic");
        assert_eq!(rows[0]["upstream_status"], "present");
        assert_eq!(rows[1]["upstream"], "refs/heads/main");
        assert_eq!(rows[1]["upstream_status"], "present");
        assert_eq!(rows[2]["upstream_status"], "gone");
        assert_eq!(rows[3]["upstream_status"], "unavailable");
        let alias = rows
            .iter()
            .find(|row| row["name"] == "refs/remotes/origin/HEAD")
            .unwrap();
        assert_eq!(alias["symbolic_target"], "refs/remotes/origin/topic");
        assert!(alias["ahead"].is_null() && alias["behind"].is_null());
        assert_eq!(rows.iter().filter(|row| row["current"] == true).count(), 1);
        assert_unchanged(&workspace.join(".git"), &before);
        let remote = run_call(
            &workspace,
            &scratch(temp.path(), 507),
            &branches(Some("remote"), Some("main"), None),
        );
        let remote_rows = remote["branches"].as_array().unwrap();
        assert_eq!(remote_rows.len(), 3);
        assert!(
            remote_rows
                .iter()
                .all(|row| row["kind"] == "remote" && row["upstream_status"] == "not_applicable")
        );

        let hidden: HiddenPaths = serde_json::from_value(json!(["^src(?:/.*)?$"])).unwrap();
        let filtered = run_filtered(
            &workspace,
            &scratch(temp.path(), 501),
            &branches(None, Some("main"), Some(1)),
            &hidden,
        );
        let row = &filtered["branches"][0];
        assert!(row["subject"].is_null() && row["ahead"].is_null() && row["behind"].is_null());
        assert_eq!(row["counts_withheld"], true);
        assert_eq!(row["subject_withheld"], true);
        assert_eq!(filtered["truncated"], true);

        host_git(&workspace, &home, &["add", ".env"]);
        host_git(
            &workspace,
            &home,
            &["commit", "-q", "-m", "protected tip canary"],
        );
        let protected = run_call(
            &workspace,
            &scratch(temp.path(), 502),
            &branches(None, None, None),
        );
        let tip = protected["branches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["current"] == true)
            .unwrap();
        assert!(tip["subject"].is_null());
        assert_eq!(tip["subject_withheld"], true);
        assert!(!protected.to_string().contains("protected tip canary"));

        std::fs::write(
            workspace.join(".git/shallow"),
            format!("{}\n", tip["oid"].as_str().unwrap()),
        )
        .unwrap();
        let shallow = run_call(
            &workspace,
            &scratch(temp.path(), 506),
            &branches(None, None, None),
        );
        let shallow_tip = shallow["branches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["current"] == true)
            .unwrap();
        assert!(shallow_tip["subject"].is_null());
        assert_eq!(shallow_tip["subject_withheld"], true);
        std::fs::remove_file(workspace.join(".git/shallow")).unwrap();

        let markers = arm_hostile(temp.path(), &workspace);
        let hostile = run_call(
            &workspace,
            &scratch(temp.path(), 503),
            &branches(None, None, Some(1)),
        );
        assert_eq!(hostile["branches"][0]["upstream_status"], "unsupported");
        assert_eq!(std::fs::read_dir(markers).unwrap().count(), 0);
        let missing = run_call(
            &workspace,
            &scratch(temp.path(), 504),
            &branches(None, Some("absent"), None),
        );
        assert!(missing["failed"].is_string());
        std::fs::write(workspace.join(".git/HEAD"), format!("{base}\n")).unwrap();
        let detached = run_call(
            &workspace,
            &scratch(temp.path(), 505),
            &branches(None, None, Some(1)),
        );
        assert_eq!(detached["branches"][0]["current"], false);
    }

    #[test]
    fn branch_discovery_keeps_non_commit_refs_and_marks_clipped_names() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        host_git(
            &workspace,
            &home,
            &["tag", "-a", "-m", "annotated tip", "annotated", "HEAD"],
        );
        let tag = std::fs::read(workspace.join(".git/refs/tags/annotated")).unwrap();
        std::fs::write(workspace.join(".git/refs/heads/annotated"), &tag).unwrap();
        std::fs::create_dir_all(workspace.join(".git/refs/remotes/review")).unwrap();
        std::fs::write(workspace.join(".git/refs/remotes/review/annotated"), &tag).unwrap();
        let result = run_call(
            &workspace,
            &scratch(temp.path(), 508),
            &branches(Some("all"), Some("main"), None),
        );
        let rows = result["branches"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .filter(|row| row["kind"] == "non-commit")
                .count(),
            2
        );
        for row in rows.iter().filter(|row| row["kind"] == "non-commit") {
            assert!(row["subject"].is_null() && row["ahead"].is_null() && row["behind"].is_null());
            assert_eq!(row["counts_withheld"], true);
            assert_eq!(row["name_truncated"], false);
        }
        let main = rows.iter().find(|row| row["current"] == true).unwrap();
        assert_eq!(main["subject"], "second commit");
        assert_eq!(main["ahead"], 0);
        assert_eq!(main["counts_withheld"], false);
        let repository = inspect(&workspace).unwrap().ok().unwrap();
        let name = format!("refs/heads/{}", "\u{00e9}".repeat(REF_BYTES));
        let raw = format!("{name}\0{}\0 \0\0commit", main["oid"].as_str().unwrap());
        let clipped = branch_row(
            raw.as_bytes(),
            None,
            &repository.git_dir,
            &PackedUpstreams::new(),
            false,
        )
        .unwrap();
        assert_eq!(clipped["name_truncated"], true);
        let shown = clipped["name"].as_str().unwrap();
        assert!(shown.len() <= REF_BYTES && name.starts_with(shown));
        let oid = main["oid"].as_str().unwrap();
        let mut packed = Vec::new();
        for name in [
            b"refs/heads/legacy-\xff".as_slice(),
            b"refs/remotes/review/legacy-\xff".as_slice(),
        ] {
            packed.extend_from_slice(oid.as_bytes());
            packed.push(b' ');
            packed.extend_from_slice(name);
            packed.push(b'\n');
        }
        std::fs::write(workspace.join(".git/packed-refs"), packed).unwrap();
        std::fs::write(
            workspace.join(".git/refs/heads/legacy-\u{fffd}"),
            format!("{oid}\n"),
        )
        .unwrap();
        let mut config = std::fs::read(workspace.join(".git/config")).unwrap();
        config.extend_from_slice(
            "\n[branch \"legacy-\u{fffd}\"]\nremote = .\nmerge = refs/heads/main\n".as_bytes(),
        );
        std::fs::write(workspace.join(".git/config"), config).unwrap();
        let lossy = run_call(
            &workspace,
            &scratch(temp.path(), 512),
            &branches(Some("all"), Some("main"), None),
        );
        assert_eq!(lossy["truncated"], true);
        let rows = lossy["branches"].as_array().unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows.iter()
                .filter(|row| row["name_truncated"] == true)
                .count(),
            2
        );
        let collision: Vec<_> = rows
            .iter()
            .filter(|row| row["name"] == "refs/heads/legacy-\u{fffd}")
            .collect();
        assert_eq!(collision.len(), 2);
        for row in collision {
            if row["name_truncated"] == true {
                assert!(row["upstream"].is_null());
                assert_eq!(row["upstream_status"], "unsupported");
            } else {
                assert_eq!(row["upstream"], "refs/heads/main");
                assert_eq!(row["upstream_status"], "present");
            }
        }
        let remote = rows
            .iter()
            .find(|row| row["name"] == "refs/remotes/review/legacy-\u{fffd}")
            .unwrap();
        assert_eq!(remote["name_truncated"], true);
        assert_eq!(remote["kind"], "remote");
        assert_eq!(remote["upstream_status"], "not_applicable");
        let main = rows.iter().find(|row| row["current"] == true).unwrap();
        assert_eq!(main["name"], "refs/heads/main");
        assert_eq!(main["name_truncated"], false);
        assert_eq!(main["subject"], "second commit");
        let mut alias = format!("refs/remotes/review/alias\0{oid}\0 \0").into_bytes();
        alias.extend_from_slice(b"refs/remotes/review/legacy-\xff\0commit");
        let alias = branch_row(
            &alias,
            None,
            &repository.git_dir,
            &PackedUpstreams::new(),
            false,
        )
        .unwrap();
        assert_eq!(alias["name_truncated"], false);
        assert_eq!(
            alias["symbolic_target"],
            "refs/remotes/review/legacy-\u{fffd}"
        );
        assert_eq!(alias["symbolic_target_truncated"], true);
        assert_eq!(alias["upstream_status"], "not_applicable");
    }

    fn import_refs(workspace: &Path, home: &Path, import: &str) {
        let input = home.join("refs.import");
        std::fs::write(&input, import).unwrap();
        let output = std::process::Command::new(discover().unwrap().executable)
            .args([
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "fast-import",
                "--quiet",
            ])
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(workspace)
            .stdin(std::fs::File::open(input).unwrap())
            .bounded_output_with_input()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture import: {:?}",
            output.status
        );
    }

    #[test]
    fn branch_divergence_caps_large_histories_without_inventing_counts() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        let parent = std::fs::read_to_string(workspace.join(".git/refs/heads/main")).unwrap();
        let mut import = String::new();
        for index in 0..=MAX_BRANCH_COUNT_COMMITS {
            import.push_str("commit refs/heads/long\ncommitter Arany Test <test@example.invalid> 1700000000 +0000\ndata 7\nbounded\n");
            if index == 0 {
                import.push_str(&format!("from {}\n", parent.trim()));
            }
            import.push('\n');
        }
        import_refs(&workspace, &home, &import);
        let result = run_call(
            &workspace,
            &scratch(temp.path(), 509),
            &branches(None, Some("main"), None),
        );
        assert_eq!(result["truncated"], true);
        let rows = result["branches"].as_array().unwrap();
        let long = rows
            .iter()
            .find(|row| row["name"] == "refs/heads/long")
            .unwrap();
        assert!(long["ahead"].is_null() && long["behind"].is_null());
        assert_eq!(long["counts_capped"], true);
        assert_eq!(long["counts_withheld"], true);
        let main = rows.iter().find(|row| row["current"] == true).unwrap();
        assert_eq!(main["ahead"], 0);
        assert_eq!(main["behind"], 0);
        assert_eq!(main["counts_capped"], false);
        let record = format!(">{}\n", main["oid"].as_str().unwrap());
        assert_eq!(
            branch_counts(record.repeat(MAX_BRANCH_COUNT_COMMITS).as_bytes()).unwrap(),
            Some((0, MAX_BRANCH_COUNT_COMMITS as u64))
        );
        assert_eq!(
            branch_counts(record.repeat(MAX_BRANCH_COUNT_COMMITS + 1).as_bytes()).unwrap(),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_git_requires_confirmed_reaping_before_continuation() {
        use std::os::unix::process::ExitStatusExt;
        let reaped = async { Ok(std::process::ExitStatus::from_raw(9)) };
        assert!(matches!(
            timeout_after_reap(reaped).await,
            ToolError::Timeout(_)
        ));
        let failed = async { Err(std::io::Error::other("reap failed")) };
        assert!(matches!(
            timeout_after_reap(failed).await,
            ToolError::Operation
        ));
        let start = tokio::time::Instant::now();
        let pending = std::future::pending();
        assert!(matches!(
            timeout_after_reap(pending).await,
            ToolError::Operation
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn branch_deadline_returns_the_completed_prefix_without_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let git = discover().unwrap();
        let hidden = none();
        let first = execute(GitReadRequest {
            workspace: &workspace,
            scratch: &scratch(temp.path(), 510),
            git: &git,
            call: &branches(None, None, Some(1)),
            hidden: &hidden,
            max_bytes: 16 * 1024,
            runtime: MAX_GIT_RUNTIME,
        })
        .await
        .unwrap();
        let first: Value = serde_json::from_str(&first).unwrap();
        let repository = inspect(&workspace).unwrap().ok().unwrap();
        let read = BranchRead {
            git: Path::new("/no-dispatch"),
            env: &[],
            workspace: &workspace,
            deadline: tokio::time::Instant::now() - Duration::from_secs(1),
            hidden: &hidden,
            repository: &repository,
            budget: 16 * 1024,
        };
        let mut rows = Rows::new(read.budget);
        assert!(rows.push(first["branches"][0].clone()));
        let pending = captured(b"must not be parsed or dispatched\n", false);
        assert!(matches!(
            read.collect(&pending, 1, None, &mut rows).await.unwrap(),
            BranchCompletion::TimedOut
        ));
        let result = branch_result(&repository, rows.rows, None, true, true);
        assert_eq!(result["branches"], first["branches"]);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["truncated"], true);
        let expired = execute(GitReadRequest {
            workspace: &workspace,
            scratch: &scratch(temp.path(), 511),
            git: &git,
            call: &branches(None, None, None),
            hidden: &hidden,
            max_bytes: 16 * 1024,
            runtime: Duration::ZERO,
        })
        .await
        .unwrap();
        let expired: Value = serde_json::from_str(&expired).unwrap();
        assert_eq!(expired["branches"], json!([]));
        assert_eq!(expired["timed_out"], true);
        assert_eq!(expired["truncated"], true);
    }

    #[test]
    fn upstream_metadata_reports_only_supported_direct_associations() {
        let direct = "[branch \"topic\"]\nremote = review\nmerge = refs/heads/topic\n";
        for (label, extra, expected) in [
            (
                "literal",
                "[remote \"review\"]\nfetch = +refs/heads/topic:refs/remotes/review/other\n",
                Ok(Some("refs/remotes/review/other".to_owned())),
            ),
            (
                "wildcard",
                "[remote \"review\"]\nfetch = refs/heads/*:refs/remotes/review/*\n",
                Ok(Some("refs/remotes/review/topic".to_owned())),
            ),
            ("missing", "", Ok(None)),
            (
                "continued section",
                "[core]\nignored = \\\n[remote \"review\"]\nfetch = refs/heads/*:refs/remotes/review/*\n",
                Err(()),
            ),
            (
                "negative",
                "[remote \"review\"]\nfetch = ^refs/heads/topic\n",
                Err(()),
            ),
            (
                "duplicate",
                "[remote \"review\"]\nfetch = refs/heads/*:refs/remotes/review/*\nfetch = refs/heads/*:refs/remotes/review/*\n",
                Err(()),
            ),
            (
                "ambiguous",
                "[branch \"topic\"]\nremote = origin\n",
                Err(()),
            ),
            ("include", "[include]\npath = outside\n", Err(())),
            (
                "conditional include",
                "[includeIf \"gitdir:somewhere\"]\npath = outside\n",
                Err(()),
            ),
            (
                "old include syntax",
                "[includeif.gitdir:somewhere]\npath = outside\n",
                Err(()),
            ),
            (
                "escaped",
                "[branch \"topic\"]\nmerge = \"refs/heads/other\\t\"\n",
                Err(()),
            ),
            (
                "continuation",
                "[remote \"review\"]\nfetch = refs/heads/*:\\\nrefs/remotes/review/*\n",
                Err(()),
            ),
            (
                "multiple wildcards",
                "[remote \"review\"]\nfetch = refs/heads/**:refs/remotes/review/**\n",
                Err(()),
            ),
        ] {
            let config = format!("{direct}{extra}");
            let actual = upstream_config(config.as_bytes())
                .ok_or(())
                .and_then(|config| config.upstream("topic"));
            assert_eq!(actual, expected, "{label}");
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        let head = host_git_output(&workspace, &home, &["rev-parse", "HEAD"]);
        let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/remotes/review/topic", &head],
        );
        host_git(&workspace, &home, &["pack-refs", "--all"]);
        let config = upstream_config(format!("{direct}[remote \"review\"]\nfetch = refs/heads/*:refs/remotes/review/*\n[branch \"alias\"]\nremote = review\nmerge = refs/heads/topic\n").as_bytes()).unwrap();
        let repository = inspect(&workspace).unwrap().ok().unwrap();
        let hidden = none();
        let read = BranchRead {
            git: Path::new("/no-dispatch"),
            env: &[],
            workspace: &workspace,
            deadline: tokio::time::Instant::now() + MAX_GIT_RUNTIME,
            hidden: &hidden,
            repository: &repository,
            budget: 16 * 1024,
        };
        let listed: &[&[u8]] = &[b"refs/heads/topic\0", b"refs/heads/alias\0"];
        let packed = read.packed_upstreams(Some(&config), listed).unwrap();
        assert_eq!(packed.len(), 1);
        std::fs::remove_file(workspace.join(".git/packed-refs")).unwrap();
        for branch in ["refs/heads/topic", "refs/heads/alias"] {
            assert_eq!(
                upstream_status(Some(&config), &repository.git_dir, &packed, branch),
                (
                    Some("refs/remotes/review/topic".into()),
                    UpstreamStatus::Present
                )
            );
        }
        std::fs::write(
            workspace.join(".git/packed-refs"),
            "invalid refs/remotes/review/topic\n",
        )
        .unwrap();
        let malformed = read.packed_upstreams(Some(&config), listed).unwrap();
        assert_eq!(
            upstream_status(
                Some(&config),
                &repository.git_dir,
                &malformed,
                "refs/heads/topic"
            ),
            (None, UpstreamStatus::Unsupported)
        );
        let loose = workspace.join(".git/refs/remotes/review/topic");
        std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
        std::fs::write(&loose, format!("{head}\n")).unwrap();
        assert_eq!(
            upstream_status(
                Some(&config),
                &repository.git_dir,
                &malformed,
                "refs/heads/topic"
            ),
            (
                Some("refs/remotes/review/topic".into()),
                UpstreamStatus::Present
            )
        );
        std::fs::remove_file(&loose).unwrap();
        std::os::unix::fs::symlink(workspace.join("src/main.rs"), &loose).unwrap();
        assert_eq!(
            upstream_status(
                Some(&config),
                &repository.git_dir,
                &packed,
                "refs/heads/topic"
            ),
            (None, UpstreamStatus::Unsupported)
        );
    }

    #[test]
    fn branch_discovery_bounds_empty_shallow_and_missing_objects() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&home).unwrap();
        host_git(&workspace, &home, &["init", "-q", "-b", "main"]);
        let empty = run_call(
            &workspace,
            &scratch(temp.path(), 600),
            &branches(None, None, None),
        );
        assert_eq!(empty["branches"], json!([]));
        assert_eq!(empty["comparison_base"], Value::Null);
        assert!(
            run_call(
                &workspace,
                &scratch(temp.path(), 601),
                &branches(None, Some("HEAD"), None)
            )["failed"]
                .is_string()
        );
        std::fs::write(workspace.join("notes.md"), "visible\n").unwrap();
        host_git(&workspace, &home, &["add", "notes.md"]);
        host_git(&workspace, &home, &["commit", "-q", "-m", "initial"]);
        let head = host_git_output(&workspace, &home, &["rev-parse", "HEAD"]);
        let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        let mut import = format!(
            "blob\nmark :1\ndata 8\nvisible\n\ncommit refs/heads/wide\ncommitter Arany Test <test@example.invalid> 1700000000 +0000\ndata 8\nwide tip\nfrom {head}\n"
        );
        for index in 0..5000 {
            import.push_str(&format!(
                "M 100644 :1 path-{index:05}-{}.txt\n",
                "a".repeat(64)
            ));
        }
        import.push('\n');
        import_refs(&workspace, &home, &import);
        let wide = run_call(
            &workspace,
            &scratch(temp.path(), 605),
            &branches(None, Some("main"), Some(2)),
        );
        assert_eq!(wide["truncated"], true);
        assert_eq!(wide["timed_out"], false);
        let rows = wide["branches"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["subject"], "initial");
        assert_eq!(rows[1]["name"], "refs/heads/wide");
        assert_eq!(rows[1]["subject"], "wide tip");
        assert_eq!(rows[1]["ahead"], 1);
        assert_eq!(rows[1]["behind"], 0);
        let prefix = format!(
            "{head}\0\0Arany\0{}\0private subject\n\n.env\ncut-path",
            1700000000
        );
        assert!(
            tip_subject(prefix.as_bytes(), true, &head, &none(), true)
                .unwrap()
                .is_none()
        );
        assert!(
            tip_subject(b"cut-header", true, &head, &none(), true)
                .unwrap()
                .is_none()
        );
        for index in 0..257 {
            std::fs::write(
                workspace.join(format!(".git/refs/heads/branch-{index:03}")),
                format!("{head}\n"),
            )
            .unwrap();
        }
        let bounded = run_call(
            &workspace,
            &scratch(temp.path(), 602),
            &branches(None, None, None),
        );
        assert!(bounded["branches"].as_array().unwrap().len() <= MAX_ROWS);
        assert_eq!(bounded["truncated"], true);
        std::fs::write(workspace.join(".git/shallow"), format!("{head}\n")).unwrap();
        let shallow = run_call(
            &workspace,
            &scratch(temp.path(), 603),
            &branches(None, Some("HEAD"), Some(1)),
        );
        assert_eq!(shallow["history_complete"], false);
        assert_eq!(shallow["branches"][0]["ahead"], 0);
        std::fs::remove_file(workspace.join(".git/shallow")).unwrap();
        std::fs::remove_file(
            workspace
                .join(".git/objects")
                .join(&head[..2])
                .join(&head[2..]),
        )
        .unwrap();
        let missing = run_call(
            &workspace,
            &scratch(temp.path(), 604),
            &branches(None, None, Some(1)),
        );
        assert!(missing["failed"].is_string());
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
        let value = status_output(
            raw.as_bytes(),
            false,
            4096,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
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

        let initial = status_output(
            b"# branch.oid (initial)\0",
            false,
            4096,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
        assert_eq!(initial["branch"]["oid"], Value::Null);

        assert!(status_output(b"1 M\0", false, 4096, &mut GitDisclosure::empty(none())).is_err());
        assert!(status_output(b"x y\0", false, 4096, &mut GitDisclosure::empty(none())).is_err());
        assert!(
            status_output(b"? a\0? b", false, 4096, &mut GitDisclosure::empty(none())).is_err()
        );
        let cut =
            status_output(b"? a\0? b", true, 4096, &mut GitDisclosure::empty(none())).unwrap();
        assert_eq!(cut["entries"], json!([{"path": "a", "state": "??"}]));
        assert_eq!(cut["truncated"], true);
    }

    #[test]
    fn tracked_ignored_paths_are_hidden_from_status_diff_and_log() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        std::fs::write(workspace.join(".gitignore"), "cfg/private.json\n").unwrap();
        std::fs::write(
            workspace.join("cfg/private.json"),
            "DISCLOSURE_CANARY=initial\n",
        )
        .unwrap();
        host_git(&workspace, &home, &["add", ".gitignore"]);
        host_git(&workspace, &home, &["add", "-f", "cfg/private.json"]);
        host_git(
            &workspace,
            &home,
            &["commit", "-q", "-m", "visible baseline"],
        );
        std::fs::write(
            workspace.join("cfg/private.json"),
            "DISCLOSURE_CANARY=hidden\n",
        )
        .unwrap();
        host_git(&workspace, &home, &["add", "-f", "cfg/private.json"]);
        host_git(
            &workspace,
            &home,
            &["commit", "-q", "-m", "DISCLOSURE_CANARY commit"],
        );
        std::fs::write(
            workspace.join("cfg/private.json"),
            "DISCLOSURE_CANARY=working\n",
        )
        .unwrap();

        let status = run_call(
            &workspace,
            &scratch(temp.path(), 801),
            &ToolCall::GitStatus { path: ".".into() },
        );
        let diff = run_call(
            &workspace,
            &scratch(temp.path(), 802),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: None,
                head: None,
                staged: false,
            },
        );
        let log = run_call(
            &workspace,
            &scratch(temp.path(), 803),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 5,
            },
        );
        let explicit = run_call(
            &workspace,
            &scratch(temp.path(), 804),
            &ToolCall::GitStatus {
                path: "cfg/private.json".into(),
            },
        );
        let output = format!("{status}{diff}{log}");
        assert_eq!(diff["files"][0]["path"], "src/a.txt");
        assert_eq!(log["commits"][0]["subject"], "visible baseline");
        assert!(!output.contains("DISCLOSURE_CANARY"), "{output}");
        assert!(!output.contains("cfg/private.json"), "{output}");
        assert!(
            status["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == "src/a.txt")
        );
        assert_eq!(explicit["entries"], json!([]));
    }

    #[test]
    fn git_disclosure_rejects_malformed_and_unsafe_ignore_controls() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join(".gitignore"), [0xff]).unwrap();
        assert!(matches!(
            GitDisclosure::new(
                &workspace,
                &none(),
                tokio::time::Instant::now() + Duration::from_secs(30),
            ),
            Err(ToolError::Path)
        ));

        std::fs::remove_file(workspace.join(".gitignore")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("missing", workspace.join(".gitignore")).unwrap();
        assert!(matches!(
            GitDisclosure::new(
                &workspace,
                &none(),
                tokio::time::Instant::now() + Duration::from_secs(30),
            ),
            Err(ToolError::FilesystemDenied)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn git_disclosure_selection_window_starts_after_delayed_capture() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join(".gitignore"), "ignored\n").unwrap();
        let action_deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        let capture = async {
            tokio::time::advance(super::super::selection::DISCOVERY_TIME + Duration::from_secs(1))
                .await;
            Ok(CapturedOutputs::Ready(Vec::new()))
        };
        let DisclosedCapture::Ready { mut disclosure, .. } =
            capture_with_disclosure(capture, &workspace, &none(), true, action_deadline)
                .await
                .unwrap()
        else {
            panic!("capture fixture must succeed");
        };
        assert!(disclosure.shown("visible", false).unwrap());
        assert!(!disclosure.shown("ignored", false).unwrap());
        tokio::time::advance(action_deadline - tokio::time::Instant::now()).await;
        assert!(matches!(
            disclosure.shown("visible", false),
            Err(ToolError::Timeout(_))
        ));
    }

    #[test]
    fn numstat_and_log_parsers_read_nul_fields() {
        let numstat = captured(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0", false);
        let value = diff_output(
            &numstat,
            &captured(b"", false),
            4096,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
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
        let log = format!(
            "{a}\0{b}\0Ana\x001700000000\0Fix é\nsrc/a.rs\n{b}\0\0Ana\x001699999999\0Root\nsrc/root.rs\n"
        );
        let value = log_output(
            log.as_bytes(),
            false,
            4096,
            LogLimit::typed(50),
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
        let commits = value["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["subject"], "Fix é");
        assert_eq!(commits[1]["parents"], json!([]));
        let without_path = log_output(
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\0Ana\x001700000000\0No path\n",
            false,
            4096,
            LogLimit::typed(50),
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
        assert_eq!(without_path["commits"], json!([]));
        assert!(
            log_output(
                format!("{}\0\0Ana\x001\0s\n", "a".repeat(39)).as_bytes(),
                false,
                4096,
                LogLimit::typed(50),
                &mut GitDisclosure::empty(none())
            )
            .is_err()
        );
    }

    #[test]
    fn log_revision_accepts_only_bounded_two_dot_ranges() {
        assert!(matches!(
            log_revision("base..head"),
            Some(LogRevision::Range {
                base: "base",
                head: "head"
            })
        ));
        assert!(matches!(log_revision("main"), Some(LogRevision::Single)));
        assert!(
            log_revision(&format!(
                "{}..{}",
                "a".repeat(MAX_REVISION_BYTES),
                "b".repeat(MAX_REVISION_BYTES)
            ))
            .is_some()
        );
        for revision in [
            "..head",
            "base..",
            "base...head",
            "base..head..other",
            "-p..head",
        ] {
            assert!(log_revision(revision).is_none(), "{revision}");
        }
        assert!(
            log_revision(&format!(
                "{}..{}",
                "a".repeat(MAX_REVISION_BYTES),
                "b".repeat(MAX_REVISION_BYTES + 1)
            ))
            .is_none()
        );
    }

    fn divergent_range_fixture(root: &Path) -> (PathBuf, PathBuf, String, String) {
        let workspace = root.join("workspace");
        let home = root.join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let git = |args: &[&str]| host_git(&workspace, &home, args);
        let oid = |revision: &str| {
            let output = host_git_output(&workspace, &home, &["rev-parse", revision]);
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(workspace.join("common.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "common"]);
        git(&["checkout", "-q", "-b", "base-only"]);
        std::fs::write(workspace.join("base.txt"), "base only\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base-only tip"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(workspace.join("head.txt"), "head only\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "head tip"]);
        let base_oid = oid("base-only");
        let head_oid = oid("HEAD");
        (workspace, home, base_oid, head_oid)
    }

    #[test]
    fn range_log_and_endpoint_diff_capture_oids_and_use_git_two_dot_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, home, base_oid, head_oid) = divergent_range_fixture(temp.path());
        let log = run_call(
            &workspace,
            &scratch(temp.path(), 1),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: Some("base-only..HEAD".into()),
                max_count: 50,
            },
        );
        assert_eq!(log["commits"].as_array().unwrap().len(), 1);
        assert_eq!(log["commits"][0]["subject"], "head tip");
        assert_eq!(
            log["comparison"],
            json!({
                "base": {"revision": "base-only", "oid": base_oid},
                "head": {"revision": "HEAD", "oid": head_oid},
            })
        );
        let empty = run_call(
            &workspace,
            &scratch(temp.path(), 2),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: Some("HEAD..HEAD".into()),
                max_count: 50,
            },
        );
        assert_eq!(empty["commits"], json!([]));
        assert_eq!(empty["truncated"], false);
        let diff = run_call(
            &workspace,
            &scratch(temp.path(), 3),
            &ToolCall::GitDiff {
                path: ".".into(),
                base: Some("base-only".into()),
                head: Some("HEAD".into()),
                staged: false,
            },
        );
        assert_eq!(diff["comparison"]["base"]["oid"], base_oid);
        assert_eq!(diff["comparison"]["head"]["oid"], head_oid);
        assert_eq!(diff["files"].as_array().unwrap().len(), 2);
        assert!(diff["patch"].as_str().unwrap().contains("base only"));
        assert!(diff["patch"].as_str().unwrap().contains("head only"));
        let three_dot = host_git_output(&workspace, &home, &["diff", "base-only...HEAD"]);
        assert!(three_dot.status.success());
        assert!(!String::from_utf8_lossy(&three_dot.stdout).contains("base only"));
        let missing = run_call(
            &workspace,
            &scratch(temp.path(), 4),
            &ToolCall::GitLog {
                path: ".".into(),
                revision: Some("missing..HEAD".into()),
                max_count: 50,
            },
        );
        assert!(missing.get("failed").is_some());
    }

    #[test]
    fn ambiguous_branch_and_tag_endpoints_fail_for_ranges_and_diffs() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, home, _, _) = divergent_range_fixture(temp.path());
        host_git(&workspace, &home, &["tag", "base-only", "HEAD"]);
        let ambiguous = host_git_output(
            &workspace,
            &home,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                "base-only^{commit}",
            ],
        );
        assert!(ambiguous.status.success());
        assert!(!ambiguous.stderr.is_empty());
        for (index, call) in [
            ToolCall::GitLog {
                path: ".".into(),
                revision: Some("base-only..HEAD".into()),
                max_count: 50,
            },
            ToolCall::GitDiff {
                path: ".".into(),
                base: Some("base-only".into()),
                head: Some("HEAD".into()),
                staged: false,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let value = run_call(&workspace, &scratch(temp.path(), index + 5), &call);
            assert_eq!(
                value,
                json!({"failed": "git revision resolution was ambiguous or unsupported"})
            );
        }
    }

    #[test]
    fn results_fit_the_budget_and_report_truncation() {
        let many: String = (0..1000).map(|n| format!("? file-{n}.txt\0")).collect();
        let value = status_output(
            many.as_bytes(),
            false,
            2048,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
        assert!(value.to_string().len() <= 2048 + ENVELOPE_BYTES);
        assert_eq!(value["truncated"], true);
        assert!(value["entries"].as_array().unwrap().len() < 1000);

        let capped: String = (0..300).map(|n| format!("? f{n}\0")).collect();
        let value = status_output(
            capped.as_bytes(),
            false,
            1 << 30,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(value["truncated"], true);

        let patch = captured(
            &format!("diff --git a/x b/x\n{}", "+x\n".repeat(10_000)).into_bytes(),
            false,
        );
        let value = diff_output(
            &captured(b"", false),
            &patch,
            4096,
            &mut GitDisclosure::empty(none()),
        )
        .unwrap();
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
            Ok(Prepared::Ready(_))
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
                    assert!(matches!(prepared, Ok(Prepared::Ready(_))), "{name}/{kind}");
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
        let value = diff_output(&numstat, &patch, 4096, &mut GitDisclosure::empty(none())).unwrap();
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
        let value = status_output(
            status.as_bytes(),
            false,
            4096,
            &mut GitDisclosure::empty(hidden.clone()),
        )
        .unwrap();
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
        let value = diff_output(
            &numstat,
            &captured(patch.as_bytes(), false),
            4096,
            &mut GitDisclosure::empty(hidden.clone()),
        )
        .unwrap();
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
            subjects(
                log_output(
                    log.as_bytes(),
                    false,
                    4096,
                    LogLimit::typed(5),
                    &mut GitDisclosure::empty(hidden.clone())
                )
                .unwrap()
            ),
            (vec!["visible".to_owned(), "later".to_owned()], false)
        );
        assert_eq!(
            subjects(
                log_output(
                    log.as_bytes(),
                    false,
                    4096,
                    LogLimit::typed(1),
                    &mut GitDisclosure::empty(hidden.clone())
                )
                .unwrap()
            ),
            (vec!["visible".to_owned()], true)
        );
        let exhausted: String = (0..MAX_FILTERED_LOG_COMMITS)
            .map(|_| commit(1, "covered", "private/s\n"))
            .collect();
        assert_eq!(
            subjects(
                log_output(
                    exhausted.as_bytes(),
                    false,
                    1 << 30,
                    LogLimit::typed(5),
                    &mut GitDisclosure::empty(hidden.clone())
                )
                .unwrap()
            ),
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

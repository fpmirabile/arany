use super::ToolError;
use super::config::Program;
use super::executable::{EXECUTABLES, git_root, has_magic};
use super::guard::GuardContext;
use super::types::{
    FORBIDDEN_NAMES, MAX_GIT_LOG_COMMITS, MAX_SNAPSHOT_BYTES, ToolCall, forbidden, hex_digest,
    valid_relative, valid_revision,
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Read;
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
        _ => false,
    }
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
        _ => return None,
    })
}

const CAPTURE_BYTES: usize = 256 * 1024;
const STDERR_BYTES: usize = 4 * 1024;
const ERROR_BYTES: usize = 1024;
const MAX_ROWS: usize = 256;
const ENVELOPE_BYTES: usize = 1024;
const HEAD_BYTES: usize = 256;
const CONFIG_BYTES: usize = 64 * 1024;
const MAX_GIT_RUNTIME: Duration = Duration::from_secs(20);
pub(super) const VIEW: &str = "git-view";
pub(super) const HOME: &str = "git-home";
const VIEW_CONFIG: &[u8] = b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfsmonitor = false\n\thooksPath = /dev/null\n\tuntrackedCache = false\n";
const LINKED: [(&str, bool, bool); 5] = [
    ("objects", true, true),
    ("refs", true, true),
    ("packed-refs", false, false),
    ("index", false, false),
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

enum Prepared {
    Ready,
    Unsupported(&'static str),
}

fn prepare_view(workspace: &Path, view: &Path) -> Result<Prepared, ToolError> {
    let root = super::fs::open_directory(workspace)?;
    match root.symlink_metadata(".git") {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(Prepared::Unsupported(UNSUPPORTED_LAYOUT)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Prepared::Unsupported(NO_REPOSITORY));
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
            return Ok(Prepared::Unsupported(UNSUPPORTED_LAYOUT));
        }
        Err(error) => return Err(error),
    };
    if !valid_head(&head) {
        return Ok(Prepared::Unsupported(UNSUPPORTED_FORMAT));
    }
    match read_optional(&git_dir, "config", CONFIG_BYTES) {
        Ok(Some(config)) if uses_extensions(&config) => {
            return Ok(Prepared::Unsupported(UNSUPPORTED_FORMAT));
        }
        Ok(_) => {}
        Err(ToolError::Limit | ToolError::Path) => {
            return Ok(Prepared::Unsupported(UNSUPPORTED_LAYOUT));
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
            _ => return Ok(Prepared::Unsupported(UNSUPPORTED_LAYOUT)),
        }
    }
    std::fs::create_dir(view).map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("config"), VIEW_CONFIG).map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("HEAD"), &head).map_err(|_| ToolError::Operation)?;
    for name in links {
        std::os::unix::fs::symlink(workspace.join(".git").join(name), view.join(name))
            .map_err(|_| ToolError::Operation)?;
    }
    Ok(Prepared::Ready)
}

fn read_optional(git_dir: &Dir, name: &str, cap: usize) -> Result<Option<Vec<u8>>, ToolError> {
    use rustix::fs::{Mode, OFlags, openat};
    match openat(
        git_dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => super::fs::read_regular(std::fs::File::from(fd), cap).map(Some),
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
    let mut specs = Vec::with_capacity(1 + 2 * (FORBIDDEN_NAMES.len() + 1));
    if !path.is_empty() && path != "." {
        specs.push(format!(":(literal){path}"));
    }
    for name in FORBIDDEN_NAMES.into_iter().chain([".env.*"]) {
        specs.push(format!(":(icase,exclude,glob)**/{name}"));
        specs.push(format!(":(icase,exclude,glob)**/{name}/**"));
    }
    specs
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn invocations(call: &ToolCall) -> Result<Vec<Vec<String>>, ToolError> {
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
            argv.push(format!("--max-count={max_count}"));
            argv.push("--format=tformat:%H%x00%P%x00%an%x00%at%x00%s".into());
            argv.push("--end-of-options".into());
            argv.extend(revision.iter().cloned());
            argv.push("--".into());
            argv.extend(pathspecs(path));
            runs.push(argv);
        }
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
    let mut child = tokio::process::Command::new(git)
        .args(argv)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ToolError::Operation)?;
    let stdout = child.stdout.take().ok_or(ToolError::Operation)?;
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let result = tokio::time::timeout_at(deadline, async {
        let ((stdout, overflow), (stderr, _)) = tokio::try_join!(
            read_prefix(stdout, CAPTURE_BYTES, false),
            read_prefix(stderr, STDERR_BYTES, true)
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
            Err(ToolError::Limit)
        }
    }
}

pub(super) async fn run(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let git = site.config.git.as_ref().ok_or(ToolError::Configuration)?;
    #[cfg(target_os = "linux")]
    let scratch = Path::new("/scratch");
    #[cfg(not(target_os = "linux"))]
    let scratch = site.root;
    let output = execute(
        site.workspace,
        scratch,
        git,
        &site.intent.call,
        site.intent.limits.result_bytes as usize,
        Duration::from_millis(u64::from(site.intent.limits.runtime_ms)),
    )
    .await;
    #[cfg(target_os = "linux")]
    site.stop_descendants().await?;
    output
}

pub(super) async fn execute(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    call: &ToolCall,
    max_bytes: usize,
    runtime: Duration,
) -> Result<String, ToolError> {
    let deadline = tokio::time::Instant::now() + runtime.min(MAX_GIT_RUNTIME);
    verify_pin(git)?;
    let view = scratch.join(VIEW);
    if let Prepared::Unsupported(message) = prepare_view(workspace, &view)? {
        return Ok(failure(message, &[], max_bytes));
    }
    let env = environment(&view, workspace, scratch);
    let mut outputs = Vec::new();
    for argv in invocations(call)? {
        let captured =
            spawn_git(Path::new(&git.executable), &argv, &env, workspace, deadline).await?;
        if !captured.overflow && captured.status != Some(0) {
            let message = captured.status.map_or_else(
                || "git terminated by a signal".to_owned(),
                |code| format!("git exited with status {code}"),
            );
            return Ok(failure(&message, &captured.stderr, max_bytes));
        }
        outputs.push(captured);
    }
    let budget = max_bytes
        .checked_sub(ENVELOPE_BYTES)
        .ok_or(ToolError::Limit)?;
    let value = match (call, outputs.as_slice()) {
        (ToolCall::GitStatus { .. }, [status]) => {
            status_output(&status.stdout, status.overflow, budget)?
        }
        (ToolCall::GitDiff { .. }, [numstat, patch]) => diff_output(numstat, patch, budget)?,
        (ToolCall::GitLog { .. }, [log]) => log_output(&log.stdout, log.overflow, budget)?,
        _ => return Err(ToolError::Configuration),
    };
    let text = value.to_string();
    if text.len() > max_bytes {
        return Err(ToolError::Limit);
    }
    Ok(text)
}

fn failure(message: &str, stderr: &[u8], max_bytes: usize) -> String {
    let stderr = String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !mentions_forbidden(line))
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

fn status_output(raw: &[u8], overflow: bool, budget: usize) -> Result<Value, ToolError> {
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
        if excluded(path) {
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

fn diff_output(numstat: &Captured, patch: &Captured, budget: usize) -> Result<Value, ToolError> {
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
        if excluded(path) {
            continue;
        }
        if !rows.push(json!({"path": path, "added": count(added)?, "deleted": count(deleted)?})) {
            truncated = true;
            break;
        }
    }
    let mut remaining = budget - rows.used;
    let mut text = String::new();
    let mut hidden = false;
    for line in String::from_utf8_lossy(&patch.stdout).split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            // The new-side path follows the last " b/"; an unparseable header hides its hunk (fail closed).
            hidden = header
                .rfind(" b/")
                .is_none_or(|at| excluded(header[at + 3..].trim_end()));
        }
        if hidden {
            continue;
        }
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

fn log_output(raw: &[u8], overflow: bool, budget: usize) -> Result<Value, ToolError> {
    let mut rows = Rows::new(budget);
    let mut truncated = overflow;
    for record in records(raw, b'\n', overflow)? {
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
        if !rows.push(row) {
            truncated = true;
            break;
        }
    }
    Ok(json!({"commits": rows.rows, "truncated": truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn host_git_output(repository: &Path, home: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new(discover().expect("pinned git").executable)
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
            .output()
            .expect("pinned git")
    }

    fn host_git(repository: &Path, home: &Path, args: &[&str]) {
        let output = host_git_output(repository, home, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Two commits, then a worktree with a modified file, a modified secret, a rewritten large file and a new file.
    fn fixture(temp: &Path) -> PathBuf {
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
        std::fs::write(workspace.join(".Env"), "SECRET_CANARY=6\n").unwrap();
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
        std::fs::write(workspace.join(".Env"), "SECRET_CANARY=7\n").unwrap();
        let rewritten: String = (0..40_000).map(|n| format!("LINE-{n:05}\n")).collect();
        std::fs::write(workspace.join("src/big.txt"), rewritten).unwrap();
        std::fs::write(workspace.join("src/new.txt"), "new\n").unwrap();
        workspace
    }

    /// Writes repository configuration and hooks that would run user code if Git ever read them.
    fn arm_hostile(temp: &Path, workspace: &Path) -> PathBuf {
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

    /// A sorted recursive snapshot of regular files, symlink targets and directories.
    fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
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

    fn scratch(temp: &Path, index: usize) -> PathBuf {
        let path = temp.join(format!("scratch-{index}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn run_call(workspace: &Path, scratch: &Path, call: &ToolCall) -> Value {
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
                16 * 1024,
                Duration::from_secs(30),
            ))
            .expect("typed Git read");
        serde_json::from_str(&output).unwrap()
    }

    struct RestoreMode(PathBuf);

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
            invocations(&ToolCall::GitStatus { path: "src".into() }).unwrap(),
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
        let diff = invocations(&ToolCall::GitDiff {
            path: "src".into(),
            base: Some("main~1".into()),
            head: Some("HEAD".into()),
            staged: false,
        })
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
        let cached = invocations(&ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: None,
            staged: true,
        })
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
            invocations(&ToolCall::GitLog {
                path: "src".into(),
                revision: Some("v1.2".into()),
                max_count: 5,
            })
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
            invocations(&ToolCall::List { path: "src".into() }),
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
        let value = status_output(raw.as_bytes(), false, 4096).unwrap();
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

        let initial = status_output(b"# branch.oid (initial)\0", false, 4096).unwrap();
        assert_eq!(initial["branch"]["oid"], Value::Null);

        assert!(status_output(b"1 M\0", false, 4096).is_err());
        assert!(status_output(b"x y\0", false, 4096).is_err());
        assert!(status_output(b"? a\0? b", false, 4096).is_err());
        let cut = status_output(b"? a\0? b", true, 4096).unwrap();
        assert_eq!(cut["entries"], json!([{"path": "a", "state": "??"}]));
        assert_eq!(cut["truncated"], true);
    }

    #[test]
    fn numstat_and_log_parsers_read_nul_fields() {
        let numstat = captured(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0", false);
        let value = diff_output(&numstat, &captured(b"", false), 4096).unwrap();
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
        let value = log_output(log.as_bytes(), false, 4096).unwrap();
        let commits = value["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["subject"], "Fix é");
        assert_eq!(commits[1]["parents"], json!([]));
        assert!(log_output(b"a\0b\0Ana\x001700000000\0", false, 4096).is_err());
        assert!(
            log_output(
                format!("{}\0\0Ana\x001\0s\n", "a".repeat(39)).as_bytes(),
                false,
                4096
            )
            .is_err()
        );
    }

    #[test]
    fn results_fit_the_budget_and_report_truncation() {
        let many: String = (0..1000).map(|n| format!("? file-{n}.txt\0")).collect();
        let value = status_output(many.as_bytes(), false, 2048).unwrap();
        assert!(value.to_string().len() <= 2048 + ENVELOPE_BYTES);
        assert_eq!(value["truncated"], true);
        assert!(value["entries"].as_array().unwrap().len() < 1000);

        let capped: String = (0..300).map(|n| format!("? f{n}\0")).collect();
        let value = status_output(capped.as_bytes(), false, 1 << 30).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(value["truncated"], true);

        let patch = captured(&"+x\n".repeat(10_000).into_bytes(), false);
        let value = diff_output(&captured(b"", false), &patch, 4096).unwrap();
        assert!(value["patch"].as_str().unwrap().ends_with('\n'));
        assert!(value.to_string().len() <= 4096 + ENVELOPE_BYTES);
        assert_eq!(value["truncated"], true);

        assert!(failure("m", &[b'x'; 5000], 200).len() <= 200);
        assert_eq!(failure("m", b"", 4096), json!({"failed": "m"}).to_string());
        let filtered = failure(
            "m",
            b"fatal: cannot read cfg/.ssh/id\nwarning: other\n",
            4096,
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
        assert_eq!(tree(&workspace.join(".git")), before);
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
        // A write would rename a private file over the view's index symlink.
        assert!(
            std::fs::symlink_metadata(view_scratch.join(VIEW).join("index"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
        let value = diff_output(&numstat, &patch, 4096).unwrap();
        assert_eq!(
            value["files"],
            json!([{"path": "src/ok.rs", "added": 1, "deleted": 1}])
        );
        let text = value["patch"].as_str().unwrap();
        assert!(text.contains("+ok"));
        assert!(!text.contains("SECRET_CANARY"));
    }
}

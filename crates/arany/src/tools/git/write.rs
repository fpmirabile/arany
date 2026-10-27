use super::super::ToolError;
use super::super::config::{GitIdentity, Program};
use super::super::permissions::HiddenPaths;
use super::super::types::{
    EffectIntent, FileChange, FileVersion, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES,
    ToolCall,
};
use super::{
    Captured, GIT_CAPTURE_BYTES, MAX_GIT_RUNTIME, REF_BYTES, Repository, VIEW, environment,
    failure, inspect, object_id, owned, pathspecs_of, read_optional, records, resolve,
    valid_branch, verify_pin, write_view_base,
};
use crate::validate::{digest, hex_digest, valid_relative};

mod frame;
mod plan;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use plan::Plan;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

// Git never writes an alternate, so nothing reaches the real `.git` before publication.
const WRITE_CONFIG: &[u8] = b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfsmonitor = false\n\thooksPath = /dev/null\n\tuntrackedCache = false\n\tlogAllRefUpdates = false\n[commit]\n\tgpgSign = false\n[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n";
const MAX_REF_ENTRIES: usize = 4096;
const MAX_OBJECTS: usize = 4096;
const MAX_LISTED_CONFLICTS: usize = 10;
// Room for the largest result and receipt; checked before any effect so the receipt always fits.
const RECEIPT_BYTES: usize = 2048;
const IN_PROGRESS: [&str; 7] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
];
const IN_PROGRESS_MESSAGE: &str = "A merge, rebase, cherry-pick, revert or bisect is in progress; finish or abort it in your own terminal first.";
const DETACHED: &str =
    "HEAD is not on a local branch; create or switch to a branch before committing.";
const DETACHED_SWITCH: &str =
    "HEAD is detached; create a branch at it first so no commit is left unreachable, then switch.";
const DRIFT: &str = "The repository changed or another Git process holds its lock; nothing was published. Inspect the repository and try again.";
const PROTECTED: &str = "Switching would change a protected, invalid or deeply nested path; switch in your own terminal instead.";
const CASE_ALIAS: &str = "Switching would change paths that differ only by letter case, or one that aliases an existing name by case; switch in your own terminal instead.";
const REF_CASE_ALIAS: &str = "A ref differing only by letter case already exists; on case-insensitive storage the new branch would overwrite it.";
const UNSUPPORTED_HEAD: &str =
    "HEAD points to a ref outside local branches; switch to a branch in your own terminal first.";
const SYMBOLIC_START: &str = "The start point names a symbolic ref such as origin/HEAD; name the branch it points to instead.";
const UNSUPPORTED_ENTRY: &str = "Switching would change a symbolic link or submodule entry, which typed Git writes do not support.";
const TYPE_CHANGE: &str = "Switching would turn a file into a directory or a directory into a file; switch in your own terminal instead.";

// What a write replaces, plus `holds`: refs it read and only locks so they cannot move meanwhile.
#[derive(Default)]
struct Publication {
    files: Vec<FileChange>,
    objects: Vec<String>,
    reference: Option<RefUpdate>,
    holds: Vec<String>,
    index: Option<Vec<u8>>,
    head: Option<Vec<u8>>,
}

// Every ref a write can depend on, compared whole at publication: HEAD, packed-refs and each loose ref.
#[derive(PartialEq)]
struct RefState {
    head: Vec<u8>,
    packed: Option<Vec<u8>>,
    loose: Vec<(String, Vec<u8>)>,
}

impl RefState {
    fn symbolic(&self, name: &str) -> bool {
        self.loose
            .iter()
            // Git's files backend treats any `ref:` prefix, whatever whitespace follows, as symbolic.
            .any(|(loose, bytes)| loose == name && bytes.starts_with(b"ref:"))
    }

    fn names(&self) -> BTreeSet<String> {
        let packed = self.packed.as_deref().unwrap_or_default();
        packed
            .split(|byte| *byte == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok()?.split_once(' '))
            .filter(|(oid, _)| !oid.starts_with(['#', '^']))
            .map(|(_, name)| name.to_owned())
            .chain(self.loose.iter().map(|(name, _)| name.clone()))
            .collect()
    }
}

struct RefUpdate {
    name: String,
    before: Option<String>,
    after: String,
}

// Err is the model-facing failure: nothing was published.
type Planned = Result<(Publication, Value), String>;

macro_rules! step {
    ($step:expr) => {
        match $step {
            Ok(value) => value,
            Err(failure) => return Ok(Err(failure)),
        }
    };
}

struct Git<'a> {
    executable: &'a Path,
    env: Vec<(&'static str, std::ffi::OsString)>,
    workspace: &'a Path,
    scratch: &'a Path,
    hidden: &'a HiddenPaths,
    view: Dir,
    deadline: tokio::time::Instant,
    max_bytes: usize,
}

impl Git<'_> {
    async fn run(
        &self,
        argv: &[&str],
        stdin: Option<&[u8]>,
        stdout_bytes: usize,
    ) -> Result<Captured, ToolError> {
        let mut full = owned(&["--no-pager"]);
        full.extend(owned(argv));
        super::spawn_git_with(
            self.executable,
            &full,
            &self.env,
            self.workspace,
            self.deadline,
            super::Io {
                stdin,
                stdout_bytes,
                writable: Some(self.scratch),
            },
        )
        .await
    }

    // A nonzero exit or overlong output becomes the model-facing failure.
    async fn checked(&self, argv: &[&str]) -> Result<Result<Captured, String>, ToolError> {
        let captured = self.run(argv, None, GIT_CAPTURE_BYTES).await?;
        Ok(if captured.overflow {
            Err(failure(
                "git output exceeded its bound",
                &[],
                self.max_bytes,
                self.hidden,
            ))
        } else if captured.status != Some(0) {
            Err(status_failure(&captured, self.max_bytes, self.hidden))
        } else {
            Ok(captured)
        })
    }
}

fn status_failure(captured: &Captured, max_bytes: usize, hidden: &HiddenPaths) -> String {
    let message = captured.status.map_or_else(
        || "git terminated by a signal".to_owned(),
        |code| format!("git exited with status {code}"),
    );
    // Git reports "nothing to commit" on stdout.
    let detail = if captured.stderr.is_empty() {
        &captured.stdout
    } else {
        &captured.stderr
    };
    failure(&message, detail, max_bytes, hidden)
}

#[cfg(any(target_os = "linux", test))]
pub(super) async fn execute(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    identity: &GitIdentity,
    intent: &EffectIntent,
    hidden: &HiddenPaths,
    runtime: Duration,
) -> Result<String, ToolError> {
    let (bytes, result) =
        match plan_frame(workspace, scratch, git, identity, intent, hidden, runtime).await? {
            Ok(planned) => planned,
            Err(failure) => return Ok(failure),
        };
    publish_frame(workspace, &bytes, result, intent, hidden)
}

pub(super) const FRAME_BYTES: usize = frame::MAX_FRAME_BYTES;

pub(super) async fn plan_frame(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    identity: &GitIdentity,
    intent: &EffectIntent,
    hidden: &HiddenPaths,
    runtime: Duration,
) -> Result<Result<(Vec<u8>, Value), String>, ToolError> {
    let (plan, result) =
        match plan::plan(workspace, scratch, git, identity, intent, hidden, runtime).await? {
            Ok(planned) => planned,
            Err(failure) => return Ok(Err(failure)),
        };
    let mut bytes = Vec::new();
    frame::write(&mut bytes, &plan)?;
    Ok(Ok((bytes, result)))
}

pub(super) fn publish_frame(
    workspace: &Path,
    bytes: &[u8],
    mut result: Value,
    intent: &EffectIntent,
    hidden: &HiddenPaths,
) -> Result<String, ToolError> {
    let plan = frame::read(&mut &*bytes)?;
    #[cfg(all(test, target_os = "linux"))]
    tests::race(workspace, tests::Stage::Planned);
    let max_bytes = intent.limits.result_bytes as usize;
    result["publication"] = match publish(workspace, &plan, intent.id)? {
        Ok(receipt) => receipt,
        Err(message) => return Ok(failure(message, &[], max_bytes, hidden)),
    };
    let text = result.to_string();
    if text.len() > max_bytes {
        // Unreachable within RECEIPT_BYTES; a published effect without its receipt is unknown.
        return Err(ToolError::Uncertain);
    }
    Ok(text)
}

fn prepare(
    repository: &Repository,
    workspace: &Path,
    view: &Path,
) -> Result<Result<RefState, &'static str>, ToolError> {
    let snapshot = match ref_state(&repository.git_dir, repository.head.clone())? {
        Ok(snapshot) => snapshot,
        Err(message) => return Ok(Err(message)),
    };
    write_view_base(repository, view, WRITE_CONFIG)?;
    let objects = view.join("objects");
    std::fs::create_dir_all(objects.join("info")).map_err(|_| ToolError::Operation)?;
    let mut alternate = workspace
        .join(".git/objects")
        .into_os_string()
        .into_encoded_bytes();
    alternate.push(b'\n');
    std::fs::write(objects.join("info/alternates"), alternate).map_err(|_| ToolError::Operation)?;
    match read_optional(&repository.git_dir, "shallow", MAX_SNAPSHOT_BYTES) {
        Ok(Some(bytes)) => {
            std::fs::write(view.join("shallow"), bytes).map_err(|_| ToolError::Operation)?;
        }
        Ok(None) => {}
        Err(ToolError::Path) => return Ok(Err(super::UNSUPPORTED_LAYOUT)),
        Err(error) => return Err(error),
    }
    if let Some(bytes) = &snapshot.packed {
        std::fs::write(view.join("packed-refs"), bytes).map_err(|_| ToolError::Operation)?;
    }
    std::fs::create_dir(view.join("refs")).map_err(|_| ToolError::Operation)?;
    for (name, bytes) in &snapshot.loose {
        let path = view.join(name);
        std::fs::create_dir_all(path.parent().ok_or(ToolError::Operation)?)
            .map_err(|_| ToolError::Operation)?;
        std::fs::write(path, bytes).map_err(|_| ToolError::Operation)?;
    }
    Ok(Ok(snapshot))
}

fn ref_state(git_dir: &Dir, head: Vec<u8>) -> Result<Result<RefState, &'static str>, ToolError> {
    let packed = match read_optional(git_dir, "packed-refs", MAX_SNAPSHOT_BYTES) {
        Ok(packed) => packed,
        Err(ToolError::Path) => return Ok(Err(super::UNSUPPORTED_LAYOUT)),
        Err(error) => return Err(error),
    };
    let refs = git_dir
        .open_dir_nofollow("refs")
        .map_err(|_| ToolError::Path)?;
    let mut loose = Vec::new();
    if let Err(message) = loose_refs(&refs, "refs", &mut loose, &mut 0)? {
        return Ok(Err(message));
    }
    loose.sort();
    Ok(Ok(RefState {
        head,
        packed,
        loose,
    }))
}

fn loose_refs(
    source: &Dir,
    prefix: &str,
    entries: &mut Vec<(String, Vec<u8>)>,
    count: &mut usize,
) -> Result<Result<(), &'static str>, ToolError> {
    for entry in source.entries().map_err(|_| ToolError::Path)? {
        let entry = entry.map_err(|_| ToolError::Path)?;
        *count += 1;
        if *count > MAX_REF_ENTRIES {
            return Err(ToolError::Limit);
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(Err(super::UNSUPPORTED_LAYOUT));
        };
        // A lock is not a ref; one held by another Git meets our own lock and refuses.
        if name.ends_with(".lock") {
            continue;
        }
        let path = format!("{prefix}/{name}");
        let kind = entry.file_type().map_err(|_| ToolError::Path)?;
        if kind.is_dir() {
            let nested = source
                .open_dir_nofollow(name)
                .map_err(|_| ToolError::Path)?;
            if let Err(message) = loose_refs(&nested, &path, entries, count)? {
                return Ok(Err(message));
            }
        } else if kind.is_file() {
            let Some(bytes) = read_optional(source, name, REF_BYTES)? else {
                return Ok(Err(super::UNSUPPORTED_LAYOUT));
            };
            entries.push((path, bytes));
        } else {
            return Ok(Err(super::UNSUPPORTED_LAYOUT));
        }
    }
    Ok(Ok(()))
}

fn branch_of(head: &[u8]) -> Option<&str> {
    let name = std::str::from_utf8(head)
        .ok()?
        .strip_suffix('\n')?
        .strip_prefix("ref: refs/heads/")?;
    valid_branch(name).then_some(name)
}

async fn commit(
    git: &Git<'_>,
    repository: &Repository,
    message: &str,
    paths: &[String],
    staged: bool,
) -> Result<Planned, ToolError> {
    let Some(branch) = branch_of(&repository.head) else {
        return Ok(Err(failure(DETACHED, &[], git.max_bytes, git.hidden)));
    };
    let name = format!("refs/heads/{branch}");
    // The parent is the snapshot's value; publication requires the live ref to still hold it.
    let before = resolve(&git.view, &name)?;
    let message = format!("--message={message}");
    let mut argv = vec![
        "commit",
        "--quiet",
        "--no-verify",
        "--no-gpg-sign",
        message.as_str(),
    ];
    // "." already means every change; another path beside it must not narrow that.
    let specs = if paths.iter().any(|path| path == ".") {
        pathspecs_of(["."])
    } else {
        pathspecs_of(paths.iter().map(String::as_str))
    };
    if !staged {
        let mut add = vec!["add", "--all", "--"];
        add.extend(specs.iter().map(String::as_str));
        step!(git.checked(&add).await?);
        argv.push("--");
        argv.extend(specs.iter().map(String::as_str));
    }
    step!(git.checked(&argv).await?);
    let after = resolve(&git.view, &name)?.ok_or(ToolError::Operation)?;
    if before.as_ref() == Some(&after) {
        return Err(ToolError::Operation);
    }
    let index =
        read_optional(&git.view, "index", MAX_SNAPSHOT_BYTES)?.ok_or(ToolError::Operation)?;
    let objects = new_objects(&git.view, index.len())?;
    Ok(Ok((
        Publication {
            objects,
            reference: Some(RefUpdate {
                name,
                before,
                after: after.clone(),
            }),
            holds: vec!["HEAD".into(), "packed-refs".into()],
            index: Some(index),
            ..Publication::default()
        },
        json!({"commit": after, "branch": branch}),
    )))
}

async fn branch(
    git: &Git<'_>,
    repository: &Repository,
    snapshot: &RefState,
    branch: &str,
    start_point: Option<&str>,
) -> Result<Planned, ToolError> {
    let name = format!("refs/heads/{branch}");
    if resolve(&git.view, &name)?.is_some() {
        return Ok(Err(failure(
            "A branch with that name already exists.",
            &[],
            git.max_bytes,
            git.hidden,
        )));
    }
    let existing = snapshot.names();
    if existing.iter().any(|other| case_alias(&name, other)) {
        return Ok(Err(failure(REF_CASE_ALIAS, &[], git.max_bytes, git.hidden)));
    }
    // A symbolic HEAD outside local branches has a referent no lock would hold.
    if repository.head.starts_with(b"ref:") && branch_of(&repository.head).is_none() {
        return Ok(Err(failure(
            UNSUPPORTED_HEAD,
            &[],
            git.max_bytes,
            git.hidden,
        )));
    }
    let starts = start_refs(&existing, &repository.head, start_point);
    // A symbolic ref's referent is not held, so only a direct ref or HEAD may name the start.
    if starts.iter().any(|start| snapshot.symbolic(start)) {
        return Ok(Err(failure(SYMBOLIC_START, &[], git.max_bytes, git.hidden)));
    }
    let mut argv = vec!["branch", "--no-track", "--end-of-options", branch];
    argv.extend(start_point);
    step!(git.checked(&argv).await?);
    let after = resolve(&git.view, &name)?.ok_or(ToolError::Operation)?;
    Ok(Ok((
        Publication {
            reference: Some(RefUpdate {
                name,
                before: None,
                after: after.clone(),
            }),
            holds: starts
                .into_iter()
                .chain(["HEAD".into(), "packed-refs".into()])
                .collect(),
            ..Publication::default()
        },
        json!({"branch": branch, "oid": after}),
    )))
}

// A ref path whose components match an existing ref case-insensitively but differ in case.
fn case_alias(name: &str, other: &str) -> bool {
    name.split('/')
        .zip(other.split('/'))
        .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
        .any(|(left, right)| left != right)
}

// HEAD, held by every write, stands for its own referent.
fn start_refs(existing: &BTreeSet<String>, head: &[u8], start_point: Option<&str>) -> Vec<String> {
    let start = start_point.unwrap_or("HEAD");
    let name = &start[..start.find(['~', '^']).unwrap_or(start.len())];
    let candidates = if name == "HEAD" {
        branch_of(head)
            .map(|branch| vec![format!("refs/heads/{branch}")])
            .unwrap_or_default()
    } else {
        vec![
            name.to_owned(),
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
            format!("refs/remotes/{name}/HEAD"),
        ]
    };
    candidates
        .into_iter()
        .filter(|candidate| existing.contains(candidate))
        .collect()
}

async fn switch(
    git: &Git<'_>,
    repository: &Repository,
    branch: &str,
) -> Result<Planned, ToolError> {
    let name = format!("refs/heads/{branch}");
    let Some(target) = resolve(&git.view, &name)? else {
        return Ok(Err(failure(
            "No local branch has that name.",
            &[],
            git.max_bytes,
            git.hidden,
        )));
    };
    let head = format!("ref: {name}\n").into_bytes();
    // No reflog is written, so commits reachable only from a detached HEAD would be lost.
    if branch_of(&repository.head).is_none() {
        return Ok(Err(failure(
            DETACHED_SWITCH,
            &[],
            git.max_bytes,
            git.hidden,
        )));
    }
    if repository.head == head {
        return Ok(Ok((
            Publication::default(),
            json!({"branch": branch, "switched": false}),
        )));
    }
    // Exit 1 only reports entries that still differ after the refresh.
    let refresh = git
        .run(
            &["update-index", "-q", "--refresh"],
            None,
            GIT_CAPTURE_BYTES,
        )
        .await?;
    if !matches!(refresh.status, Some(0 | 1)) {
        return Ok(Err(status_failure(&refresh, git.max_bytes, git.hidden)));
    }
    let dirty = step!(
        git.checked(&[
            "diff-index",
            "-z",
            "--name-only",
            "--no-renames",
            "--ignore-submodules=all",
            "HEAD",
            "--",
        ])
        .await?
    );
    let dirty: BTreeSet<&[u8]> = records(&dirty.stdout, 0, false)?.into_iter().collect();
    let changes = step!(
        git.checked(&[
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "--no-renames",
            "--ignore-submodules=all",
            "--end-of-options",
            "HEAD",
            target.as_str(),
        ])
        .await?
    );
    let fields = records(&changes.stdout, 0, false)?;
    if fields.len() % 2 != 0 {
        return Err(ToolError::Operation);
    }
    if fields.len() / 2 > MAX_SNAPSHOT_FILES {
        return Err(ToolError::Limit);
    }
    #[cfg(all(test, target_os = "linux"))]
    tests::race(git.workspace, tests::Stage::Checked);
    let (mut files, blobs) = step!(changed_files(git, &fields, &dirty)?);
    for (content, (_, executable, index)) in read_blobs(git, &blobs).await?.into_iter().zip(&blobs)
    {
        files[*index].after = Some(FileVersion::from_bytes(&content, *executable));
    }
    step!(
        git.checked(&[
            "read-tree",
            "-m",
            "--end-of-options",
            "HEAD",
            target.as_str()
        ])
        .await?
    );
    let index =
        read_optional(&git.view, "index", MAX_SNAPSHOT_BYTES)?.ok_or(ToolError::Operation)?;
    let count = files.len();
    let mut holds = vec![name];
    holds.extend(branch_of(&repository.head).map(|source| format!("refs/heads/{source}")));
    holds.push("packed-refs".into());
    Ok(Ok((
        Publication {
            files,
            holds,
            index: Some(index),
            head: Some(head),
            ..Publication::default()
        },
        json!({"branch": branch, "switched": true, "files": count}),
    )))
}

// A blob to read: its object name, whether it is executable and the index of its file.
type Blob<'a> = (&'a str, bool, usize);
type ChangedFiles<'a> = (Vec<FileChange>, Vec<Blob<'a>>);

// The switch's file changes with verified preimages and the blobs to read, or the refusal.
fn changed_files<'a>(
    git: &Git<'_>,
    fields: &[&'a [u8]],
    dirty: &BTreeSet<&[u8]>,
) -> Result<Result<ChangedFiles<'a>, String>, ToolError> {
    let root = super::super::fs::open_directory(git.workspace)?;
    let mut files = Vec::with_capacity(fields.len() / 2);
    let mut blobs = Vec::new();
    let mut conflicts = Vec::new();
    for pair in fields.chunks(2) {
        let header = std::str::from_utf8(pair[0]).map_err(|_| ToolError::Operation)?;
        let [old_mode, new_mode, old_oid, new_oid, _] = header
            .strip_prefix(':')
            .ok_or(ToolError::Operation)?
            .split(' ')
            .collect::<Vec<_>>()[..]
        else {
            return Err(ToolError::Operation);
        };
        let Some(path) = std::str::from_utf8(pair[1])
            .ok()
            .filter(|path| valid_relative(path, false) && path.split('/').count() <= 16)
        else {
            return Ok(Err(failure(PROTECTED, &[], git.max_bytes, git.hidden)));
        };
        let (Some(old), Some(new)) = (entry_mode(old_mode), entry_mode(new_mode)) else {
            return Ok(Err(failure(
                UNSUPPORTED_ENTRY,
                &[],
                git.max_bytes,
                git.hidden,
            )));
        };
        if dirty.contains(path.as_bytes()) {
            conflicts.push(path);
            continue;
        }
        let before = match old {
            Some(executable) => match current_version(&root, path, old_oid, executable)? {
                Some(version) => Some(version),
                // Edited after the cleanliness check: that edit must not become the preimage.
                None => {
                    conflicts.push(path);
                    continue;
                }
            },
            None if fields.as_chunks::<2>().0.iter().any(|removed| {
                removed[0].split(|byte| *byte == b' ').nth(1) == Some(b"000000".as_slice())
                    && (path
                        .as_bytes()
                        .strip_prefix(removed[1])
                        .is_some_and(|rest| rest.starts_with(b"/"))
                        || removed[1]
                            .strip_prefix(path.as_bytes())
                            .is_some_and(|rest| rest.starts_with(b"/")))
            }) =>
            {
                return Ok(Err(failure(TYPE_CHANGE, &[], git.max_bytes, git.hidden)));
            }
            // An untracked file or directory at an added path would be overwritten.
            None if root
                .symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            None => {
                conflicts.push(path);
                continue;
            }
        };
        if let Some(executable) = new {
            blobs.push((object_id(new_oid.as_bytes())?, executable, files.len()));
        }
        files.push(FileChange {
            path: path.to_owned(),
            before,
            after: None,
        });
    }
    if !conflicts.is_empty() {
        // Paths a Read rule hides are counted but never named.
        let listed = conflicts
            .iter()
            .filter(|path| !git.hidden.covers(path))
            .take(MAX_LISTED_CONFLICTS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(Err(failure(
            &format!(
                "Switching would overwrite local changes or untracked files in {} path(s): {listed}. Commit, move or remove them first; nothing changed.",
                conflicts.len()
            ),
            &[],
            git.max_bytes,
            git.hidden,
        )));
    }
    // ASCII-case aliases collide on case-insensitive volumes.
    let mut prefixes = std::collections::BTreeMap::new();
    for file in &files {
        for (end, _) in file.path.match_indices('/').chain([(file.path.len(), "")]) {
            let prefix = &file.path[..end];
            if *prefixes
                .entry(prefix.to_ascii_lowercase())
                .or_insert(prefix)
                != prefix
            {
                return Ok(Err(failure(CASE_ALIAS, &[], git.max_bytes, git.hidden)));
            }
        }
    }
    Ok(Ok((files, blobs)))
}

// None: absent; Some(executable) for a regular blob. Links and submodules are unsupported.
fn entry_mode(mode: &str) -> Option<Option<bool>> {
    match mode {
        "000000" => Some(None),
        "100644" => Some(Some(false)),
        "100755" => Some(Some(true)),
        _ => None,
    }
}

// The tracked file's bytes and mode, only when they still equal HEAD's blob; None otherwise.
fn current_version(
    root: &Dir,
    path: &str,
    blob: &str,
    blob_executable: bool,
) -> Result<Option<FileVersion>, ToolError> {
    use std::os::unix::fs::PermissionsExt;
    let file = super::super::fs::open_file(root, path)?;
    let executable = file
        .metadata()
        .map_err(|_| ToolError::Operation)?
        .permissions()
        .mode()
        & 0o111
        != 0;
    let bytes = super::super::fs::read_regular(file, MAX_FILE_BYTES)?;
    if executable != blob_executable || blob_id(&bytes) != blob {
        return Ok(None);
    }
    // Publication compares only the digest and executable bit of a before version.
    Ok(Some(FileVersion {
        sha256: hex_digest(&bytes),
        content: String::new(),
        executable,
    }))
}

// Git's SHA-1 object name of a blob; SHA-256 repositories are refused as unsupported.
fn blob_id(bytes: &[u8]) -> String {
    let mut context = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY);
    context.update(format!("blob {}\0", bytes.len()).as_bytes());
    context.update(bytes);
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn read_blobs(
    git: &Git<'_>,
    blobs: &[(&str, bool, usize)],
) -> Result<Vec<Vec<u8>>, ToolError> {
    if blobs.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = blobs.iter().map(|(oid, ..)| format!("{oid}\n")).collect();
    let cap = MAX_SNAPSHOT_BYTES + blobs.len() * 64;
    let captured = git
        .run(&["cat-file", "--batch"], Some(input.as_bytes()), cap)
        .await?;
    if captured.overflow {
        return Err(ToolError::Limit);
    }
    if captured.status != Some(0) {
        return Err(ToolError::Operation);
    }
    let mut rest = captured.stdout.as_slice();
    let mut total = 0usize;
    let mut contents = Vec::with_capacity(blobs.len());
    for (oid, ..) in blobs {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(ToolError::Operation)?;
        let header = std::str::from_utf8(&rest[..end]).map_err(|_| ToolError::Operation)?;
        let size = header
            .strip_prefix(*oid)
            .and_then(|tail| tail.strip_prefix(" blob "))
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or(ToolError::Operation)?;
        total = total.saturating_add(size);
        if size > MAX_FILE_BYTES || total > MAX_SNAPSHOT_BYTES {
            return Err(ToolError::Limit);
        }
        let body = rest
            .get(end + 1..end + 1 + size)
            .ok_or(ToolError::Operation)?;
        if rest.get(end + 1 + size) != Some(&b'\n') {
            return Err(ToolError::Operation);
        }
        contents.push(body.to_vec());
        rest = &rest[end + 2 + size..];
    }
    if !rest.is_empty() {
        return Err(ToolError::Operation);
    }
    Ok(contents)
}

// Loose objects Git wrote into the private store, as "xx/yyyy…" names.
fn new_objects(view: &Dir, index_bytes: usize) -> Result<Vec<String>, ToolError> {
    let objects = view
        .open_dir_nofollow("objects")
        .map_err(|_| ToolError::Operation)?;
    let mut names = Vec::new();
    let mut total = index_bytes;
    for entry in objects.entries().map_err(|_| ToolError::Operation)? {
        let entry = entry.map_err(|_| ToolError::Operation)?;
        let prefix = entry
            .file_name()
            .into_string()
            .map_err(|_| ToolError::Operation)?;
        let directory = objects
            .open_dir_nofollow(&prefix)
            .map_err(|_| ToolError::Operation)?;
        // Packs appear only through maintenance, which the view config disables.
        if prefix == "info"
            || prefix == "pack"
                && directory
                    .entries()
                    .is_ok_and(|mut entries| entries.next().is_none())
        {
            continue;
        }
        if prefix.len() != 2 || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            return Err(ToolError::Operation);
        }
        for object in directory.entries().map_err(|_| ToolError::Operation)? {
            let object = object.map_err(|_| ToolError::Operation)?;
            let rest = object
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Operation)?;
            let name = format!("{prefix}/{rest}");
            let metadata = object.metadata().map_err(|_| ToolError::Operation)?;
            object_id(name.replace('/', "").as_bytes())?;
            total = total.saturating_add(metadata.len() as usize);
            if !metadata.is_file() {
                return Err(ToolError::Operation);
            }
            if names.len() == MAX_OBJECTS || total > MAX_SNAPSHOT_BYTES {
                return Err(ToolError::Limit);
            }
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

// A Git-compatible `<path>.lock`: exclusive creation, removed unless committed by rename.
struct Lock {
    directory: Dir,
    leaf: String,
    path: String,
    file: Option<cap_std::fs::File>,
    committed: bool,
    // Directories this lock created, outermost first, removed again unless it commits.
    created: Vec<(Dir, String)>,
}

impl Lock {
    fn acquire(git_dir: &Dir, path: &str) -> Result<Self, ToolError> {
        let mut directory = git_dir.try_clone().map_err(|_| ToolError::Operation)?;
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().ok_or(ToolError::Operation)?;
        let mut created = Vec::new();
        // Any failure after a directory was created removes it again, as an uncommitted lock does.
        let opened = (|| {
            for part in parts {
                exact_case(&directory, part)?;
                match directory.open_dir_nofollow(part) {
                    Ok(next) => directory = next,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // A new nested branch needs its directory, as Git creates it.
                        let parent = directory.try_clone().map_err(|_| ToolError::Operation)?;
                        directory
                            .create_dir(part)
                            .map_err(|_| ToolError::Conflict)?;
                        created.push((parent, part.to_owned()));
                        // The new directory must survive a crash once its ref is acknowledged.
                        sync_directory(&directory).map_err(|_| ToolError::Operation)?;
                        directory = directory
                            .open_dir_nofollow(part)
                            .map_err(|_| ToolError::Conflict)?;
                    }
                    Err(_) => return Err(ToolError::Conflict),
                }
            }
            exact_case(&directory, leaf)?;
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o644);
            }
            let file = directory
                .open_with(format!("{leaf}.lock"), &options)
                .map_err(|_| ToolError::Conflict)?;
            Ok((directory, file))
        })();
        let (directory, file) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                remove_created(&created);
                return Err(error);
            }
        };
        Ok(Self {
            directory,
            leaf: leaf.to_owned(),
            path: format!("{path}.lock"),
            file: Some(file),
            committed: false,
            created,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        let file = self.file.as_mut().ok_or(ToolError::Operation)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| ToolError::Operation)
    }

    fn identity(&self) -> Result<(u64, u64), ToolError> {
        use cap_std::fs::MetadataExt;
        let metadata = self
            .file
            .as_ref()
            .ok_or(ToolError::Operation)?
            .metadata()
            .map_err(|_| ToolError::Operation)?;
        Ok((metadata.dev(), metadata.ino()))
    }

    fn commit(&mut self) -> Result<(), ToolError> {
        self.file = None;
        self.directory
            .rename(format!("{}.lock", self.leaf), &self.directory, &self.leaf)
            .map_err(|_| ToolError::Uncertain)?;
        self.committed = true;
        sync_directory(&self.directory)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.directory.remove_file(format!("{}.lock", self.leaf));
            remove_created(&self.created);
        }
    }
}

// A directory another Git populated meanwhile is not empty and stays.
fn remove_created(created: &[(Dir, String)]) {
    for (parent, name) in created.iter().rev() {
        let _ = parent.remove_dir(name);
    }
}

// An ASCII-case variant of an existing entry would alias it on case-insensitive storage.
fn exact_case(directory: &Dir, name: &str) -> Result<(), ToolError> {
    for (count, entry) in directory
        .entries()
        .map_err(|_| ToolError::Conflict)?
        .enumerate()
    {
        if count >= MAX_REF_ENTRIES {
            return Err(ToolError::Limit);
        }
        let other = entry.map_err(|_| ToolError::Conflict)?.file_name();
        if other
            .to_str()
            .is_none_or(|other| other != name && other.eq_ignore_ascii_case(name))
        {
            return Err(ToolError::Conflict);
        }
    }
    Ok(())
}

const LEDGER_BYTES: usize = 64 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LockRecord {
    path: String,
    device: u64,
    inode: u64,
    sha256: String,
}

fn ledger_name(intent: uuid::Uuid) -> String {
    format!("arany-locks-{intent}.json")
}

// Durable record of one write's locks: a dead Guard's locks are reported, never removed.
struct Ledger<'a> {
    git_dir: &'a Dir,
    name: String,
    locks: Vec<LockRecord>,
}

impl<'a> Ledger<'a> {
    fn create(git_dir: &'a Dir, intent: uuid::Uuid) -> Result<Self, ToolError> {
        let ledger = Self {
            git_dir,
            name: ledger_name(intent),
            locks: Vec::new(),
        };
        ledger.save()?;
        Ok(ledger)
    }

    fn record(&mut self, lock: &Lock, content: &[u8]) -> Result<(), ToolError> {
        let (device, inode) = lock.identity()?;
        self.locks.push(LockRecord {
            path: lock.path.clone(),
            device,
            inode,
            sha256: hex_digest(content),
        });
        self.save()
    }

    fn save(&self) -> Result<(), ToolError> {
        let temporary = format!("{}.tmp", self.name);
        let bytes = serde_json::to_vec(&self.locks).map_err(|_| ToolError::Operation)?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .truncate(true)
            .follow(FollowSymlinks::No);
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.git_dir
            .open_with(&temporary, &options)
            .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()))
            .and_then(|()| self.git_dir.rename(&temporary, self.git_dir, &self.name))
            .map_err(|_| ToolError::Operation)?;
        sync_directory(self.git_dir)
    }
}

impl Drop for Ledger<'_> {
    fn drop(&mut self) {
        let _ = self.git_dir.remove_file(format!("{}.tmp", self.name));
        let _ = self.git_dir.remove_file(&self.name);
    }
}

// A planted record must not carry controls or bidirectional overrides into the report.
fn valid_lock_path(path: &str) -> bool {
    matches!(path, "HEAD.lock" | "index.lock" | "packed-refs.lock")
        || path.len() <= 512
            && path.ends_with(".lock")
            && path.strip_prefix("refs/").is_some_and(|rest| {
                rest.split('/').all(|part| {
                    !part.is_empty()
                        && part != "."
                        && part != ".."
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                })
            })
}

// A dead Guard's locks cannot be proven exclusively ours, so they are only reported, never moved.
pub(crate) fn recover_locks(workspace: &Path, intent: uuid::Uuid) -> Result<(), ToolError> {
    let root = super::super::fs::open_directory(workspace)?;
    let Ok(git_dir) = root.open_dir_nofollow(".git") else {
        return Ok(());
    };
    let name = ledger_name(intent);
    let Some(bytes) = read_optional(&git_dir, &name, LEDGER_BYTES)? else {
        let _ = git_dir.remove_file(format!("{name}.tmp"));
        return Ok(());
    };
    let locks: Vec<LockRecord> =
        serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
    let mut stale = Vec::new();
    for record in locks {
        if !valid_lock_path(&record.path) {
            return Err(ToolError::Operation);
        }
        let (parent, leaf) = record.path.rsplit_once('/').unwrap_or(("", &record.path));
        let mut directory = git_dir.try_clone().map_err(|_| ToolError::Operation)?;
        let mut present = true;
        for part in parent.split('/').filter(|part| !part.is_empty()) {
            match directory.open_dir_nofollow(part) {
                Ok(next) => directory = next,
                Err(_) => {
                    present = false;
                    break;
                }
            }
        }
        // Another inode or other bytes at the path belong to someone else and are not reported.
        let ours = present
            && super::open_optional(&directory, leaf)?.is_some_and(|file| {
                use std::os::unix::fs::MetadataExt;
                file.metadata().is_ok_and(|metadata| {
                    (metadata.dev(), metadata.ino()) == (record.device, record.inode)
                }) && super::super::fs::read_regular(file, MAX_SNAPSHOT_BYTES)
                    .is_ok_and(|bytes| hex_digest(&bytes) == record.sha256)
            });
        if ours {
            stale.push(format!(".git/{}", record.path));
        }
    }
    if !stale.is_empty() {
        return Err(ToolError::StaleGitLocks(stale.join(", ")));
    }
    git_dir
        .remove_file(&name)
        .map_err(|_| ToolError::Operation)?;
    let _ = git_dir.remove_file(format!("{name}.tmp"));
    sync_directory(&git_dir)
}

// Reopen "." readably: a path-only capability handle cannot prove durability through fsync.
fn sync_directory(directory: &Dir) -> Result<(), ToolError> {
    super::super::fs::open_file(directory, ".")
        .and_then(|file| file.sync_all().map_err(|_| ToolError::Uncertain))
        .map_err(|_| ToolError::Uncertain)
}

// After the first effect any failure is Uncertain.
fn publish(
    workspace: &Path,
    plan: &Plan,
    intent: uuid::Uuid,
) -> Result<Result<Value, &'static str>, ToolError> {
    let publication = &plan.publication;
    let root = super::super::fs::open_directory(workspace)?;
    let Ok(git_dir) = root.open_dir_nofollow(".git") else {
        return Ok(Err(DRIFT));
    };
    if super::super::fs::identity(&git_dir)? != plan.identity {
        return Ok(Err(DRIFT));
    }
    let index_before = plan.index_before.map(plan::hex);
    let mut head_before = Vec::new();
    // Declared before the locks so it outlives them: its record goes only once they are gone.
    let mut ledger = Ledger::create(&git_dir, intent)?;
    let locked = (|| {
        let mut take = |path: &str, content: Option<&[u8]>| {
            let mut lock = Lock::acquire(&git_dir, path)?;
            if let Some(content) = content {
                lock.write(content)?;
            }
            ledger.record(&lock, content.unwrap_or_default())?;
            Ok::<_, ToolError>(lock)
        };
        let reference = publication
            .reference
            .as_ref()
            .map(|update| take(&update.name, Some(format!("{}\n", update.after).as_bytes())))
            .transpose()?;
        let holds = publication
            .holds
            .iter()
            .map(|name| take(name, None))
            .collect::<Result<Vec<_>, _>>()?;
        let index = publication
            .index
            .as_ref()
            .map(|bytes| take("index", Some(bytes)))
            .transpose()?;
        let head = publication
            .head
            .as_ref()
            .map(|bytes| take("HEAD", Some(bytes)))
            .transpose()?;
        let live_head =
            read_optional(&git_dir, "HEAD", super::HEAD_BYTES)?.ok_or(ToolError::Conflict)?;
        let live_index = read_optional(&git_dir, "index", MAX_SNAPSHOT_BYTES)?;
        head_before = live_head.clone();
        if !matches!(ref_state(&git_dir, live_head)?, Ok(live) if plan.refs == plan::RefDigests::from(&live))
            || live_index.as_deref().map(hex_digest) != index_before
        {
            return Err(ToolError::Conflict);
        }
        Ok::<_, ToolError>((reference, holds, index, head))
    })();
    // Held locks stay until every replaced target is published, then drop and disappear.
    let (reference, _holds, index, head) = match locked {
        Ok(locks) => locks,
        Err(ToolError::Conflict | ToolError::Path | ToolError::NotFound) => return Ok(Err(DRIFT)),
        Err(error) => return Err(error),
    };
    #[cfg(all(test, target_os = "linux"))]
    tests::race(workspace, tests::Stage::Locked);
    if !publication.files.is_empty() {
        // The shared publisher's exact-case admission: never write through a case alias.
        match super::super::fs::worker_publication_paths(&root, &publication.files) {
            Ok(()) => {}
            Err(ToolError::Path) => return Ok(Err(CASE_ALIAS)),
            Err(error) => return Err(error),
        }
        // Validates every before digest first and reports Uncertain itself after a partial change.
        match super::super::fs::apply_files(&root, &publication.files) {
            Ok(_) => {}
            Err(ToolError::Conflict) => return Ok(Err(DRIFT)),
            Err(error) => return Err(error),
        }
        #[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
        crate::fault::git_write::published(intent);
    }
    let effects = (|| {
        copy_objects(&git_dir, plan)?;
        for mut lock in [reference, index, head].into_iter().flatten() {
            lock.commit()?;
            #[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
            crate::fault::git_write::published(intent);
        }
        Ok::<_, ToolError>(())
    })();
    let affected = !publication.files.is_empty()
        || !publication.objects.is_empty()
        || publication.reference.is_some()
        || publication.index.is_some()
        || publication.head.is_some();
    if effects.is_err() && affected {
        return Err(ToolError::Uncertain);
    }
    effects?;
    let mut receipt = json!({
        "objects": publication.objects.len(),
        "files": publication.files.len(),
    });
    if let Some(update) = &publication.reference {
        receipt["ref"] =
            json!({"name": update.name, "before": update.before, "after": update.after});
    }
    if let Some(bytes) = &publication.index {
        receipt["index"] = json!({"before": index_before, "after": hex_digest(bytes)});
    }
    if let Some(bytes) = &publication.head {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim_end().to_owned();
        receipt["head"] = json!({"before": text(&head_before), "after": text(bytes)});
    }
    Ok(Ok(receipt))
}

fn copy_objects(git_dir: &Dir, plan: &Plan) -> Result<(), ToolError> {
    if plan.publication.objects.is_empty() {
        return Ok(());
    }
    let objects = git_dir
        .open_dir_nofollow("objects")
        .map_err(|_| ToolError::Operation)?;
    for (name, bytes) in plan.publication.objects.iter().zip(&plan.objects) {
        let (prefix, rest) = name.split_once('/').ok_or(ToolError::Operation)?;
        let directory = match objects.open_dir_nofollow(prefix) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                objects
                    .create_dir(prefix)
                    .map_err(|_| ToolError::Operation)?;
                sync_directory(&objects)?;
                objects
                    .open_dir_nofollow(prefix)
                    .map_err(|_| ToolError::Operation)?
            }
            Err(_) => return Err(ToolError::Operation),
        };
        // Objects are content-addressed: an existing one already holds these bytes.
        if directory.symlink_metadata(rest).is_ok() {
            continue;
        }
        let temporary = format!("tmp_obj_arany_{}", uuid::Uuid::now_v7());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o444);
        }
        let written = directory
            .open_with(&temporary, &options)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
            .map_err(|_| ToolError::Operation)
            .and_then(|()| {
                match rustix::fs::renameat_with(
                    &directory,
                    &temporary,
                    &directory,
                    rest,
                    rustix::fs::RenameFlags::NOREPLACE,
                ) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
                    Err(_) => Err(ToolError::Operation),
                }
            });
        let _ = directory.remove_file(&temporary);
        written?;
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
pub(super) mod tests;

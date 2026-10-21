use super::super::ToolError;
use super::super::config::{GitIdentity, Program};
use super::super::types::{
    FileChange, FileVersion, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES, ToolCall,
    hex_digest, valid_relative,
};
use super::{
    CAPTURE_BYTES, Captured, MAX_GIT_RUNTIME, Repository, VIEW, environment, failure, inspect,
    object_id, owned, pathspecs_of, read_optional, records, valid_branch, verify_pin,
    write_view_base,
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

// Objects go to a private directory that borrows the real store as an alternate; Git never writes
// an alternate, so nothing reaches the real `.git` before publication.
const WRITE_CONFIG: &[u8] = b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfsmonitor = false\n\thooksPath = /dev/null\n\tuntrackedCache = false\n\tlogAllRefUpdates = false\n[commit]\n\tgpgSign = false\n[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n";
const MAX_REF_ENTRIES: usize = 4096;
const REF_BYTES: usize = 1024;
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
const DRIFT: &str = "The repository changed or another Git process holds its lock; nothing was published. Inspect the repository and try again.";
const PROTECTED: &str = "Switching would change a protected, invalid or deeply nested path; switch in your own terminal instead.";
const CASE_ALIAS: &str = "Switching would change paths that differ only by letter case, or one that aliases an existing name by case; switch in your own terminal instead.";
const UNSUPPORTED_ENTRY: &str = "Switching would change a symbolic link or submodule entry, which typed Git writes do not support.";

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
        let captured = self.run(argv, None, CAPTURE_BYTES).await?;
        Ok(if captured.overflow {
            Err(failure(
                "git output exceeded its bound",
                &[],
                self.max_bytes,
            ))
        } else if captured.status != Some(0) {
            Err(status_failure(&captured, self.max_bytes))
        } else {
            Ok(captured)
        })
    }
}

fn status_failure(captured: &Captured, max_bytes: usize) -> String {
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
    failure(&message, detail, max_bytes)
}

pub(super) async fn execute(
    workspace: &Path,
    scratch: &Path,
    git: &Program,
    identity: &GitIdentity,
    call: &ToolCall,
    max_bytes: usize,
    runtime: Duration,
) -> Result<String, ToolError> {
    if max_bytes < RECEIPT_BYTES {
        return Err(ToolError::Limit);
    }
    let deadline = tokio::time::Instant::now() + runtime.min(MAX_GIT_RUNTIME);
    verify_pin(git)?;
    let repository = match inspect(workspace)? {
        Ok(repository) => repository,
        Err(message) => return Ok(failure(message, &[], max_bytes)),
    };
    if !matches!(call, ToolCall::GitBranch { .. })
        && IN_PROGRESS
            .iter()
            .any(|name| repository.git_dir.symlink_metadata(name).is_ok())
    {
        return Ok(failure(IN_PROGRESS_MESSAGE, &[], max_bytes));
    }
    let view = scratch.join(VIEW);
    let snapshot = match prepare(&repository, workspace, &view)? {
        Ok(snapshot) => snapshot,
        Err(message) => return Ok(failure(message, &[], max_bytes)),
    };
    #[cfg(all(test, target_os = "linux"))]
    tests::race(workspace, tests::Stage::Snapshot);
    let mut env = environment(&view, workspace, scratch);
    for (key, value) in [
        ("GIT_AUTHOR_NAME", &identity.name),
        ("GIT_AUTHOR_EMAIL", &identity.email),
        ("GIT_COMMITTER_NAME", &identity.name),
        ("GIT_COMMITTER_EMAIL", &identity.email),
    ] {
        env.push((key, value.into()));
    }
    let git = Git {
        executable: Path::new(&git.executable),
        env,
        workspace,
        scratch,
        view: Dir::open_ambient_dir(&view, cap_std::ambient_authority())
            .map_err(|_| ToolError::Operation)?,
        deadline,
        max_bytes,
    };
    let planned = match call {
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => commit(&git, &repository, message, paths, *staged).await?,
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
        } => branch(&git, name, start_point.as_deref()).await?,
        ToolCall::GitSwitch { branch } => switch(&git, &repository, branch).await?,
        _ => return Err(ToolError::Configuration),
    };
    let (publication, mut result) = match planned {
        Ok(planned) => planned,
        Err(failure) => return Ok(failure),
    };
    #[cfg(all(test, target_os = "linux"))]
    tests::race(workspace, tests::Stage::Planned);
    result["publication"] =
        match publish(workspace, &git.view, &repository, &snapshot, &publication)? {
            Ok(receipt) => receipt,
            Err(message) => return Ok(failure(message, &[], max_bytes)),
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
        return Ok(Err(failure(DETACHED, &[], git.max_bytes)));
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
    let specs = pathspecs_of(paths.iter().map(String::as_str));
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
    branch: &str,
    start_point: Option<&str>,
) -> Result<Planned, ToolError> {
    let name = format!("refs/heads/{branch}");
    if resolve(&git.view, &name)?.is_some() {
        return Ok(Err(failure(
            "A branch with that name already exists.",
            &[],
            git.max_bytes,
        )));
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
            holds: vec!["HEAD".into(), "packed-refs".into()],
            ..Publication::default()
        },
        json!({"branch": branch, "oid": after}),
    )))
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
        )));
    };
    let head = format!("ref: {name}\n").into_bytes();
    if repository.head == head {
        return Ok(Ok((
            Publication::default(),
            json!({"branch": branch, "switched": false}),
        )));
    }
    // Exit 1 only reports entries that still differ after the refresh.
    let refresh = git
        .run(&["update-index", "-q", "--refresh"], None, CAPTURE_BYTES)
        .await?;
    if !matches!(refresh.status, Some(0 | 1)) {
        return Ok(Err(status_failure(&refresh, git.max_bytes)));
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

// The switch's file changes with verified preimages and the blobs to read, or the refusal.
fn changed_files<'a>(
    git: &Git<'_>,
    fields: &[&'a [u8]],
    dirty: &BTreeSet<&[u8]>,
) -> Result<Result<(Vec<FileChange>, Vec<Blob<'a>>), String>, ToolError> {
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
            return Ok(Err(failure(PROTECTED, &[], git.max_bytes)));
        };
        let (Some(old), Some(new)) = (entry_mode(old_mode), entry_mode(new_mode)) else {
            return Ok(Err(failure(UNSUPPORTED_ENTRY, &[], git.max_bytes)));
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
        let listed = conflicts
            .iter()
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
        )));
    }
    // Paths differing only by ASCII case would alias each other on a case-insensitive volume.
    let folded: BTreeSet<String> = files
        .iter()
        .map(|file| file.path.to_ascii_lowercase())
        .collect();
    if folded.len() != files.len() {
        return Ok(Err(failure(CASE_ALIAS, &[], git.max_bytes)));
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

// A branch's object name, from its loose file or else `packed-refs`, never following links.
fn resolve(git_dir: &Dir, name: &str) -> Result<Option<String>, ToolError> {
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

// A Git-compatible `<path>.lock`: exclusive creation, removed unless committed by rename.
struct Lock {
    directory: Dir,
    leaf: String,
    file: Option<cap_std::fs::File>,
    committed: bool,
}

impl Lock {
    fn acquire(git_dir: &Dir, path: &str) -> Result<Self, ToolError> {
        let mut directory = git_dir.try_clone().map_err(|_| ToolError::Operation)?;
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().ok_or(ToolError::Operation)?;
        for part in parts {
            match directory.open_dir_nofollow(part) {
                Ok(next) => directory = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // A new nested branch needs its directory, as Git creates it.
                    directory
                        .create_dir(part)
                        .map_err(|_| ToolError::Conflict)?;
                    directory = directory
                        .open_dir_nofollow(part)
                        .map_err(|_| ToolError::Conflict)?;
                }
                Err(_) => return Err(ToolError::Conflict),
            }
        }
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
        Ok(Self {
            directory,
            leaf: leaf.to_owned(),
            file: Some(file),
            committed: false,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        let file = self.file.as_mut().ok_or(ToolError::Operation)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| ToolError::Operation)
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
        }
    }
}

// Reopen "." readably: a path-only capability handle cannot prove durability through fsync.
fn sync_directory(directory: &Dir) -> Result<(), ToolError> {
    super::super::fs::open_file(directory, ".")
        .and_then(|file| file.sync_all().map_err(|_| ToolError::Uncertain))
        .map_err(|_| ToolError::Uncertain)
}

// Locks every replaced or depended-on target, then requires the live refs, HEAD and index to equal
// the snapshot Git worked from, and publishes files, objects, the ref, the index and HEAD in that
// order. After the first effect any failure is Uncertain.
fn publish(
    workspace: &Path,
    view: &Dir,
    repository: &Repository,
    snapshot: &RefState,
    publication: &Publication,
) -> Result<Result<Value, &'static str>, ToolError> {
    let root = super::super::fs::open_directory(workspace)?;
    let Ok(git_dir) = root.open_dir_nofollow(".git") else {
        return Ok(Err(DRIFT));
    };
    if super::super::fs::identity(&git_dir)? != repository.identity {
        return Ok(Err(DRIFT));
    }
    let index_before = repository
        .index
        .as_ref()
        .map(|(bytes, _)| hex_digest(bytes));
    let locked = (|| {
        let mut reference = None;
        if let Some(update) = &publication.reference {
            let mut lock = Lock::acquire(&git_dir, &update.name)?;
            lock.write(format!("{}\n", update.after).as_bytes())?;
            reference = Some(lock);
        }
        let holds = publication
            .holds
            .iter()
            .map(|name| Lock::acquire(&git_dir, name))
            .collect::<Result<Vec<_>, _>>()?;
        let index = publication
            .index
            .as_ref()
            .map(|bytes| {
                let mut lock = Lock::acquire(&git_dir, "index")?;
                lock.write(bytes)?;
                Ok::<_, ToolError>(lock)
            })
            .transpose()?;
        let head = publication
            .head
            .as_ref()
            .map(|bytes| {
                let mut lock = Lock::acquire(&git_dir, "HEAD")?;
                lock.write(bytes)?;
                Ok::<_, ToolError>(lock)
            })
            .transpose()?;
        let live_head =
            read_optional(&git_dir, "HEAD", super::HEAD_BYTES)?.ok_or(ToolError::Conflict)?;
        let live_index = read_optional(&git_dir, "index", MAX_SNAPSHOT_BYTES)?;
        if !matches!(ref_state(&git_dir, live_head)?, Ok(live) if live == *snapshot)
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
    }
    let effects = (|| {
        copy_objects(view, &git_dir, &publication.objects)?;
        for mut lock in [reference, index, head].into_iter().flatten() {
            lock.commit()?;
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
        receipt["head"] = json!({"before": text(&repository.head), "after": text(bytes)});
    }
    Ok(Ok(receipt))
}

fn copy_objects(view: &Dir, git_dir: &Dir, names: &[String]) -> Result<(), ToolError> {
    if names.is_empty() {
        return Ok(());
    }
    let source = view
        .open_dir_nofollow("objects")
        .map_err(|_| ToolError::Operation)?;
    let objects = git_dir
        .open_dir_nofollow("objects")
        .map_err(|_| ToolError::Operation)?;
    for name in names {
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
        let bytes = super::super::fs::read_regular(
            source
                .open_with(
                    name,
                    OpenOptions::new().read(true).follow(FollowSymlinks::No),
                )
                .map_err(|_| ToolError::Operation)?
                .into_std(),
            MAX_SNAPSHOT_BYTES,
        )?;
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
            .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()))
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

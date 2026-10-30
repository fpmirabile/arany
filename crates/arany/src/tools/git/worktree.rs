use super::super::ToolError;
use super::super::types::MAX_FILE_BYTES;
use super::runner::{Environment, Git};
use super::{object_id, records};
use crate::session::rewind::FileState;
use crate::validate::forbidden;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// A bounded wait for any one worktree step; the whole creation also sits under the editor time bound.
const STEP_TIME: std::time::Duration = std::time::Duration::from_secs(120);
const LIST_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const PREFIX: &str = "arany-";

// An editor's linked worktree: its Git name (`arany-<assignment>`) and the base commit it checked out.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Worktree {
    pub(crate) name: String,
    pub(crate) base: String,
}

// What a new worktree needs from the Workspace: its record, the base's tracked files and sizes, and
// the untracked, non-ignored files to copy in.
pub(crate) type Created = (Worktree, Vec<(String, u64)>, Vec<String>);

// Tracked paths with their base versions, and the untracked paths now in the tree.
pub(crate) type Changed = (Vec<(String, Option<FileState>)>, Vec<String>);

fn owned(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| (*arg).to_owned()).collect()
}

// Worktree steps run on the editor store's blocking thread, each bounded by its own watchdog.
fn query(git: &Git<'_>, argv: &[&str], cap: usize) -> Result<(Option<i32>, Vec<u8>), ToolError> {
    let (status, stdout, overflow) = git.output_blocking(&owned(argv), STEP_TIME, cap)?;
    if overflow {
        return Err(ToolError::Limit);
    }
    Ok((status, stdout))
}

fn output(git: &Git<'_>, argv: &[&str]) -> Result<Vec<u8>, ToolError> {
    match query(git, argv, LIST_BYTES)? {
        (Some(0), stdout) => Ok(stdout),
        _ => Err(ToolError::Operation),
    }
}

fn paths(listing: &[u8]) -> Result<Vec<String>, ToolError> {
    records(listing, 0, false)?
        .into_iter()
        .map(|path| String::from_utf8(path.to_vec()).map_err(|_| ToolError::Path))
        .collect()
}

fn common_dir(git: &Git<'_>) -> Result<PathBuf, ToolError> {
    let common = String::from_utf8(output(git, &["rev-parse", "--git-common-dir"])?)
        .map_err(|_| ToolError::Path)?;
    Ok(git.work_tree.join(common.trim_end()))
}

// Git for a worktree names its private Git directory explicitly; the tree's own `.git` file is editor-writable.
pub(crate) fn worktree_git<'a>(
    program: &'a super::super::config::Program,
    environment: &'a Environment,
    tree: &Path,
    worktree: &Worktree,
) -> Result<Git<'a>, ToolError> {
    let mut git = Git::workspace(program, environment);
    git.git_dir = common_dir(&git)?.join("worktrees").join(&worktree.name);
    git.work_tree = tree.to_owned();
    Ok(git)
}

// The base is the working tree as a stash commit, or HEAD when the tree is clean; an unborn HEAD has none.
fn base(git: &Git<'_>) -> Result<Option<String>, ToolError> {
    let stash = output(git, &["stash", "create"])?;
    let stash = String::from_utf8_lossy(&stash).trim_end().to_owned();
    if !stash.is_empty() {
        return Ok(Some(object_id(stash.as_bytes())?.to_owned()));
    }
    match query(
        git,
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        LIST_BYTES,
    )? {
        (Some(0), head) => Ok(Some(
            object_id(head.strip_suffix(b"\n").unwrap_or(&head))?.to_owned(),
        )),
        _ => Ok(None),
    }
}

// The base's tracked files with their sizes, for the disk check and the checkout fix-ups.
fn tracked(git: &Git<'_>, base: &str) -> Result<Vec<(String, u64)>, ToolError> {
    let listing = output(git, &["ls-tree", "-r", "-l", "-z", "--full-tree", base])?;
    let mut files = Vec::new();
    for record in records(&listing, 0, false)? {
        let (meta, path) = std::str::from_utf8(record)
            .ok()
            .and_then(|record| record.split_once('\t'))
            .ok_or(ToolError::Path)?;
        let size = meta
            .split_whitespace()
            .nth(3)
            .and_then(|size| size.parse().ok())
            .unwrap_or(0);
        files.push((path.to_owned(), size));
    }
    Ok(files)
}

fn untracked(git: &Git<'_>) -> Result<Vec<String>, ToolError> {
    paths(&output(
        git,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?)
}

// Checks out the base into `tree` with hooks off and LFS content left as pointers; `None` without a base.
pub(crate) fn create(
    program: &super::super::config::Program,
    environment: &Environment,
    tree: &Path,
    reserve: impl FnOnce(u64) -> Result<(), ToolError>,
) -> Result<Option<Created>, ToolError> {
    let mut git = Git::workspace(program, environment);
    if git.admit_blocking()?.is_some() {
        return Err(ToolError::Configuration);
    }
    let Some(base) = base(&git)? else {
        return Ok(None);
    };
    let files = tracked(&git, &base)?;
    let untracked = untracked(&git)?;
    reserve(files.iter().map(|(_, size)| size).sum())?;
    let name = tree
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with(PREFIX))
        .ok_or(ToolError::Path)?
        .to_owned();
    git.skip_lfs_smudge = true;
    let tree_arg = tree.to_str().ok_or(ToolError::Path)?;
    output(
        &git,
        &["worktree", "add", "--quiet", "--detach", tree_arg, &base],
    )?;
    Ok(Some((Worktree { name, base }, files, untracked)))
}

// Removes Arany's own worktree; a refusal (such as an edited `.git` file) falls back to deleting it directly.
pub(crate) fn remove(
    program: &super::super::config::Program,
    environment: &Environment,
    tree: &Path,
    worktree: &Worktree,
) {
    let git = Git::workspace(program, environment);
    let removed = tree
        .to_str()
        .is_some_and(|tree| output(&git, &["worktree", "remove", "--force", tree]).is_ok());
    if !removed && worktree.name.starts_with(PREFIX) {
        let _ = std::fs::remove_dir_all(tree);
        if let Ok(common) = common_dir(&git) {
            let _ = std::fs::remove_dir_all(common.join("worktrees").join(&worktree.name));
        }
    }
}

// Points Git's record of a renamed worktree at its new tree, as `git worktree repair` would.
pub(crate) fn relocate(
    program: &super::super::config::Program,
    environment: &Environment,
    tree: &Path,
    worktree: &Worktree,
) -> Result<(), ToolError> {
    let git = worktree_git(program, environment, tree, worktree)?;
    let mut target = tree.as_os_str().to_owned();
    target.push("/.git\n");
    std::fs::write(git.git_dir.join("gitdir"), target.as_encoded_bytes())
        .map_err(|_| ToolError::Operation)
}

pub(crate) fn changed(git: &Git<'_>, base: &str) -> Result<Changed, ToolError> {
    if git.admit_blocking()?.is_some() {
        return Err(ToolError::Configuration);
    }
    let raw = output(
        git,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--ignore-submodules=all",
            base,
            "--",
        ],
    )?;
    let fields = records(&raw, 0, false)?;
    let mut tracked = Vec::new();
    for pair in fields.chunks(2) {
        let [meta, path] = pair else {
            return Err(ToolError::Operation);
        };
        let meta = std::str::from_utf8(meta).map_err(|_| ToolError::Path)?;
        let path = std::str::from_utf8(path).map_err(|_| ToolError::Path)?;
        let mut parts = meta.trim_start_matches(':').split(' ');
        let (Some(mode), Some(_), Some(oid)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(ToolError::Operation);
        };
        // Protected files left the tree at creation and never integrate.
        if path.split('/').any(forbidden) {
            continue;
        }
        let before = if mode == "000000" {
            None
        } else {
            Some(blob_state(git, oid, mode == "100755")?)
        };
        tracked.push((path.to_owned(), before));
    }
    Ok((tracked, untracked(git)?))
}

fn blob_state(git: &Git<'_>, oid: &str, executable: bool) -> Result<FileState, ToolError> {
    let (status, content) = query(git, &["cat-file", "blob", oid], MAX_FILE_BYTES)?;
    if status != Some(0) {
        return Err(ToolError::Operation);
    }
    Ok(FileState {
        sha256: crate::validate::hex_digest(&content),
        bytes: u32::try_from(content.len()).map_err(|_| ToolError::Limit)?,
        executable,
    })
}

#[cfg(test)]
mod tests;

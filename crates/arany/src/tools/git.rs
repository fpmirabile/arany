mod hooks;
pub(crate) mod host;
mod runner;
mod worktree;
mod write;

pub(crate) use hooks::Hooks;
pub(crate) use runner::{Environment, resolve, stale_locks};
pub(crate) use worktree::{PREFIX as WORKTREE_PREFIX, Worktree};

use super::permissions::HiddenPaths;
use super::selection::{CONTROL_BYTES, Selector};
use super::types::{
    Attestation, EffectIntent, GitReceipt, MAX_GIT_LOG_COMMITS, ToolCall, ToolDisposition,
    ToolObservation,
};
use super::{Cancellation, ToolError, ToolRuntime};
use crate::validate::{
    FORBIDDEN_NAMES, MAX_REVISION_BYTES, MAX_REVISION_RANGE_BYTES, forbidden, valid_relative,
    valid_revision,
};
use cap_fs_ext::MetadataExt as CapMetadataExt;
use cap_std::fs::Dir;
use runner::{Captured, GIT_CAPTURE_BYTES, Git, Io, STDERR_BYTES};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

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

const ERROR_BYTES: usize = 1024;
const MAX_ROWS: usize = 256;
const MAX_SUBJECT_CHARS: usize = 256;
const MAX_FILTERED_LOG_COMMITS: u32 = 1000;
const MAX_BRANCH_COUNT_COMMITS: usize = 1000;
const DISCLOSURE_CONTRACT: &[u8] = b";typed-git-ignore-disclosure-v1:status-diff-log-paths-and-patches-filtered-before-bounds;tracked-ignored-paths-hidden;log-subjects-require-visible-path-evidence;path-read-stderr-suppressed;replay-never-reruns-selection";
// Semantic unit fixtures allow slow native spawns; production retains its row deadline.
const MAX_BRANCH_ROW_RUNTIME: Duration = if cfg!(test) {
    Duration::from_secs(10)
} else {
    Duration::from_secs(1)
};
const BRANCH_CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const ENVELOPE_BYTES: usize = 1024;
const REF_BYTES: usize = 1024;
const MAX_GIT_RUNTIME: Duration = Duration::from_millis(super::types::GIT_TIMEOUT_MS as u64);
const NO_REPOSITORY: &str =
    "The Workspace root has no .git; typed Git needs a repository rooted at the Workspace.";
const GIT_CHANGED: &str =
    "The Git pinned for this Run changed on disk; nothing ran. The next Run pins the new Git.";

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
    Ok(match call {
        ToolCall::GitStatus { path } => vec![status_argv(path)],
        ToolCall::GitDiff {
            path,
            base,
            head,
            staged,
        } => diff_argvs(path, base.as_deref(), head.as_deref(), *staged),
        ToolCall::GitLog {
            path,
            revision,
            max_count,
        } => vec![log_argv(path, revision.as_deref(), *max_count, filtered)],
        ToolCall::GitBranch {
            name: None,
            scope,
            max_count,
            ..
        } => vec![branch_list_argv(scope.as_deref(), *max_count)?],
        _ => return Err(ToolError::Configuration),
    })
}

fn status_argv(path: &str) -> Vec<String> {
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
    argv
}

fn diff_argvs(
    path: &str,
    base: Option<&str>,
    head: Option<&str>,
    staged: bool,
) -> Vec<Vec<String>> {
    let mut runs = Vec::new();
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
        if staged {
            argv.push("--cached".into());
        }
        argv.push("--end-of-options".into());
        argv.extend(
            base.iter()
                .chain(head.iter())
                .map(|revision| (*revision).to_owned()),
        );
        argv.push("--".into());
        argv.extend(pathspecs(path));
        runs.push(argv);
    }

    runs
}

fn log_argv(path: &str, revision: Option<&str>, max_count: u32, filtered: bool) -> Vec<String> {
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
    argv.extend(revision.map(str::to_owned));
    argv.push("--".into());
    argv.extend(pathspecs(path));
    argv
}

fn branch_list_argv(scope: Option<&str>, max_count: Option<u32>) -> Result<Vec<String>, ToolError> {
    let mut argv = owned(&[
        "--no-pager",
        "--no-optional-locks",
        "for-each-ref",
        "--sort=refname",
        "--format=%(refname)%00%(objectname)%00%(HEAD)%00%(symref)%00%(objecttype)%00%(upstream)%00%(upstream:track,nobracket)",
    ]);
    argv.push(format!(
        "--count={}",
        max_count.unwrap_or(MAX_ROWS as u32) + 1
    ));
    match scope.unwrap_or("local") {
        "local" => argv.push("refs/heads/".into()),
        "remote" => argv.push("refs/remotes/".into()),
        "all" => argv.extend(owned(&["refs/heads/", "refs/remotes/"])),
        _ => return Err(ToolError::Configuration),
    }
    Ok(argv)
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
    git: &Git<'_>,
    revision: &str,
    deadline: tokio::time::Instant,
) -> Result<Captured, ToolError> {
    let expression = format!("{revision}^{{commit}}");
    git.run(
        &owned(&[
            "--no-pager",
            "--no-optional-locks",
            "rev-parse",
            "--verify",
            "--end-of-options",
            &expression,
        ]),
        Io::default(),
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
    git: &'a Git<'a>,
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
            let capture = resolve_revision(self.git, revision, self.deadline).await?;
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
    git: &'a Git<'a>,
    call: &'a ToolCall,
    hidden: &'a HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
}

impl PreparedGitRead<'_> {
    async fn run(self) -> Result<String, ToolError> {
        let budget = self
            .max_bytes
            .checked_sub(ENVELOPE_BYTES)
            .ok_or(ToolError::Limit)?;
        let comparison = match (ComparisonContext {
            git: self.git,
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
        let history_complete = match call {
            ToolCall::GitBranch { .. } => match self.history_complete().await {
                Ok(complete) => complete,
                Err(ToolError::Timeout(_)) => {
                    return Ok(branch_result(false, Vec::new(), None, true, true).to_string());
                }
                Err(error) => return Err(error),
            },
            _ => true,
        };
        let captured = capture_with_disclosure(
            self.capture_outputs(&call, history_complete),
            &self.git.work_tree,
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
            .render(&call, &outputs, history_complete, budget, &mut disclosure)
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

    // Shallow history makes ancestry and parentless tips incomplete.
    async fn history_complete(&self) -> Result<bool, ToolError> {
        let captured = self
            .git
            .run(
                &owned(&["rev-parse", "--is-shallow-repository"]),
                Io::default(),
                self.deadline,
            )
            .await?;
        Ok(captured.stdout != b"true\n")
    }

    async fn capture_outputs(
        &self,
        call: &ToolCall,
        history_complete: bool,
    ) -> Result<CapturedOutputs, ToolError> {
        let branch_read = matches!(call, ToolCall::GitBranch { name: None, .. });
        let mut outputs = Vec::new();
        let filtered = !self.hidden.is_empty() || matches!(call, ToolCall::GitLog { .. });
        for argv in invocations(call, filtered)? {
            let capture = match self.git.run(&argv, Io::default(), self.deadline).await {
                Err(ToolError::Timeout(_)) if branch_read => {
                    return Ok(CapturedOutputs::Failed(
                        branch_result(history_complete, Vec::new(), None, true, true).to_string(),
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
        history_complete: bool,
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
                    git: self.git,
                    deadline: self.deadline,
                    hidden: self.hidden,
                    history_complete,
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

const WRITES_LEFT_TO_USER: &str = "Commits, branch creation and switches are left to the user; leave changes in the working tree.";
const HOOKS_NOT_RUN: &str = "Repository hooks did not run: the user has not accepted them for this folder, or they changed since; accept them with /permissions in the terminal.";

// With accepted hooks and no human to ask, a Git write is offered only where a user-owned allow rule covers it.
pub(super) fn offers_write(runtime: &ToolRuntime, call: &ToolCall) -> bool {
    use super::permissions::{Decision, PermissionSource};
    runtime.git_hooks.is_none()
        || runtime.human_approvals
        || runtime.permissions().is_some_and(|config| {
            let (decision, basis, _) = config.evaluate_skill_file(call, None);
            decision == Decision::Allow
                && matches!(
                    basis,
                    PermissionSource::Rule | PermissionSource::TypedPrefixRule
                )
        })
}

pub(super) fn catalog(runtime: &ToolRuntime) -> Value {
    let write = runtime.config.write;
    let offered = |name: &str| {
        let call = match name {
            "git_commit" => ToolCall::GitCommit {
                message: "commit".into(),
                paths: vec![".".into()],
                staged: false,
            },
            "git_switch" => ToolCall::GitSwitch {
                branch: "main".into(),
            },
            _ => ToolCall::GitBranch {
                name: Some("branch".into()),
                start_point: None,
                scope: None,
                base: None,
                max_count: None,
            },
        };
        offers_write(runtime, &call)
    };
    let branch_create = write && offered("git_branch");
    let operations: Vec<_> = super::catalog::git_operations(write, true)
        .into_iter()
        .filter(|name| !matches!(*name, "git_push" | "gh_pr_create") || write)
        .filter(|name| {
            !matches!(*name, "gh_pr_create" | "gh_pr_list" | "gh_pr_view")
                || runtime.config.pinned_gh().is_some()
        })
        .filter(|name| !matches!(*name, "git_commit" | "git_switch") || offered(name))
        .collect();
    let local_writes = operations
        .iter()
        .any(|name| matches!(*name, "git_commit" | "git_switch"));
    let mut value = json!({"write": branch_create || local_writes, "host": true,
        "operations": operations, "scope": super::catalog::GIT_SCOPE});
    if write && !branch_create {
        value["branch_create"] = json!(false);
    }
    if write && !(branch_create && offered("git_commit") && offered("git_switch")) {
        value["writes"] = json!(WRITES_LEFT_TO_USER);
    }
    value
}

// Editors in a Git repository work in a linked worktree; these wrap its lifecycle for the editor store.
pub(super) fn create_worktree(
    runtime: &ToolRuntime,
    directory: &Path,
    tree_name: &str,
) -> Result<Option<worktree::Created>, ToolError> {
    let (Some(program), Some(environment)) = (
        runtime.config.git.as_ref(),
        runtime.git_environment.as_deref(),
    ) else {
        return Ok(None);
    };
    if std::fs::symlink_metadata(environment.workspace().join(".git")).is_err() {
        return Ok(None);
    }
    let parent = super::fs::open_directory(directory)?;
    let reserve = |bytes| super::fs::editor::require_reserve(&parent, bytes);
    worktree::create(program, environment, &directory.join(tree_name), reserve)
}

pub(super) fn worktree_changes(
    runtime: &ToolRuntime,
    tree: &Path,
    record: &Worktree,
) -> Result<worktree::Changed, ToolError> {
    let (Some(program), Some(environment)) = (
        runtime.config.git.as_ref(),
        runtime.git_environment.as_deref(),
    ) else {
        return Err(ToolError::Configuration);
    };
    let git = worktree::worktree_git(program, environment, tree, record)?;
    worktree::changed(&git, &record.base)
}

pub(super) fn remove_worktree(runtime: &ToolRuntime, tree: &Path, record: &Worktree) {
    if let (Some(program), Some(environment)) = (
        runtime.config.git.as_ref(),
        runtime.git_environment.as_deref(),
    ) {
        worktree::remove(program, environment, tree, record);
    }
}

pub(super) fn relocate_worktree(
    runtime: &ToolRuntime,
    tree: &Path,
    record: &Worktree,
) -> Result<(), ToolError> {
    let (Some(program), Some(environment)) = (
        runtime.config.git.as_ref(),
        runtime.git_environment.as_deref(),
    ) else {
        return Err(ToolError::Configuration);
    };
    worktree::relocate(program, environment, tree, record)
}

pub(super) async fn folder_hooks(workspace: &Path, editors: &Path) -> Option<Hooks> {
    let environment = Environment::capture(workspace, editors)?;
    let program = resolve(&environment)?;
    hooks::detect(&Git::workspace(&program, &environment))
        .await
        .ok()?
}

pub(super) async fn accepted_hooks(
    runtime: &ToolRuntime,
    accepted: Option<[u8; 32]>,
) -> Option<Hooks> {
    let accepted = accepted?;
    let git = Git::workspace(
        runtime.config.git.as_ref()?,
        runtime.git_environment.as_deref()?,
    );
    let hooks = hooks::detect(&git).await.ok()??;
    (hooks.digest == accepted).then_some(hooks)
}

// Binds Git intents to the cage and disclosure contract they ran under.
pub(super) fn enforcement_digest() -> [u8; 32] {
    let mut contract = b"arany-native-git-v1:user-configuration;cage-hooks-off-fsmonitor-off-pager-cat-editor-false-no-network;workspace-config-and-programs-refused".to_vec();
    contract.extend_from_slice(DISCLOSURE_CONTRACT);
    crate::validate::digest(&contract)
}

// The Workspace's `.git` directory, opened without following links.
fn git_directory(workspace: &Path) -> Result<Dir, ToolError> {
    use cap_fs_ext::DirExt;
    super::fs::open_directory(workspace)?
        .open_dir_nofollow(".git")
        .map_err(|_| ToolError::Path)
}

async fn execute(
    git: &Git<'_>,
    call: &ToolCall,
    hidden: &HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
) -> Result<String, ToolError> {
    // A branch read keeps time to tear down and deliver the rows it already completed.
    let deadline = if matches!(call, ToolCall::GitBranch { name: None, .. }) {
        deadline - BRANCH_CLEANUP_RESERVE
    } else {
        deadline
    };
    PreparedGitRead {
        git,
        call,
        hidden,
        max_bytes,
        deadline,
    }
    .run()
    .await
}

fn not_run(intent: EffectIntent, error: ToolError) -> ToolObservation {
    match error {
        ToolError::ChangedInput => refused(intent, json!({"failed": GIT_CHANGED}).to_string()),
        error => super::unstarted_observation(intent, error),
    }
}

fn refused(intent: EffectIntent, output: String) -> ToolObservation {
    observation(intent, ToolDisposition::Failed, output, None, None)
}

fn observation(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: String,
    receipt: Option<GitReceipt>,
    limit_cause: Option<super::types::LimitCause>,
) -> ToolObservation {
    ToolObservation {
        spill: None,
        intent,
        disposition,
        output,
        attestation: receipt.map(Attestation::Git),
        cache_update: super::CacheUpdate::NotApplicable,
        enforcement_cause: None,
        limit_cause,
        approval_cause: None,
        guard_stage: None,
    }
}

// A write whose Git process did not finish cleanly may have changed the repository.
fn uncertain(intent: EffectIntent, workspace: &Path) -> ToolObservation {
    let locks = stale_locks(workspace);
    let mut output =
        "Git write outcome unknown; inspect the repository before continuing. Do not retry automatically."
            .to_owned();
    if !locks.is_empty() {
        output = format!("{}. {output}", ToolError::StaleGitLocks(locks.join(", ")));
    }
    observation(intent, ToolDisposition::Uncertain, output, None, None)
}

// A write runs the folder's hooks only while they match what the user accepted; otherwise it says so.
async fn write_hooks(
    runtime: &ToolRuntime,
    git: &Git<'_>,
) -> (Option<std::path::PathBuf>, Option<&'static str>) {
    match hooks::detect(git).await {
        Ok(Some(found)) if runtime.git_hooks.as_deref() == Some(&found) => {
            (Some(found.directory), None)
        }
        Ok(Some(_)) | Err(_) => (None, Some(HOOKS_NOT_RUN)),
        Ok(None) => (None, None),
    }
}

fn with_hooks_note(
    mut written: write::Written,
    note: Option<&str>,
    max_bytes: usize,
) -> write::Written {
    if let Some(note) = note
        && let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(&written.output)
    {
        fields.insert("hooks_not_run".into(), json!(note));
        let output = Value::Object(fields).to_string();
        if output.len() <= max_bytes {
            written.output = output;
        }
    }
    written
}

// Typed Git runs natively in the Engine under the cage, with the user's configuration.
pub(super) async fn run(
    runtime: &ToolRuntime,
    intent: EffectIntent,
    mut cancellation: Cancellation,
) -> ToolObservation {
    let (Some(program), Some(environment)) = (
        runtime.config.git.as_ref(),
        runtime.git_environment.as_deref(),
    ) else {
        return super::unstarted_observation(intent, ToolError::Configuration);
    };
    let started = tokio::time::Instant::now();
    let window = match runtime.begin_active_action(started).await {
        Ok(window) => window,
        Err(error) => return super::unstarted_observation(intent, error),
    };
    let deadline_after = |limit: Duration| {
        window.map_or(started + limit, |window| {
            (started + limit).min(window.payload_deadline)
        })
    };
    let mut deadline = deadline_after(
        Duration::from_millis(u64::from(intent.effective_timeout_ms())).min(MAX_GIT_RUNTIME),
    );
    if let Err(error) = runtime
        .before_dispatch(window, intent.limits.runtime_ms, &cancellation)
        .await
    {
        return super::unstarted_observation(intent, error);
    }
    let hidden = runtime.hidden_paths(&intent.call);
    let max_bytes = intent.limits.result_bytes as usize;
    let mut git = Git::workspace(program, environment);
    if std::fs::symlink_metadata(&git.git_dir).is_err() {
        return refused(intent, failure(NO_REPOSITORY, &[], max_bytes, &hidden));
    }
    let admitted = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Cancelled),
        admitted = git.admit(deadline) => admitted,
    };
    match admitted {
        Ok(None) => {}
        Ok(Some(refusal)) => return refused(intent, failure(&refusal, &[], max_bytes, &hidden)),
        Err(error) => return not_run(intent, error),
    }
    let mutates = intent.call.mutates();
    let mut hooks_note = None;
    if mutates {
        (git.hooks, hooks_note) = write_hooks(runtime, &git).await;
        // Accepted hooks may run a project's checks, so the write gets the command runtime ceiling.
        if git.hooks.is_some() {
            deadline = deadline_after(Duration::from_millis(u64::from(intent.limits.runtime_ms)));
        }
    }
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Cancelled),
        result = execute_typed(&git, &intent.call, &hidden, max_bytes, deadline) => result,
    };
    let written = match result {
        Ok(written) => with_hooks_note(written, hooks_note, max_bytes),
        Err(error) => return failed_run(intent, error, environment.workspace()),
    };
    let receipt = (!mutates || written.refs.is_some()).then(|| GitReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        executable_sha256: program.sha256.clone(),
        exit_code: written.exit_code,
        refs: written.refs,
    });
    let (disposition, output, _, limit_cause) =
        super::dispatch::outcome(&intent, Ok(written.output));
    observation(intent, disposition, output, receipt, limit_cause)
}

async fn execute_typed(
    git: &Git<'_>,
    call: &ToolCall,
    hidden: &HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
) -> Result<write::Written, ToolError> {
    if call.mutates() {
        write::execute(git, call, hidden, max_bytes, deadline).await
    } else {
        execute(git, call, hidden, max_bytes, deadline)
            .await
            .map(|output| write::Written {
                output,
                refs: None,
                exit_code: git.exit_code(),
            })
    }
}

// A failed write's effect is unknown; a read that failed changed nothing.
fn failed_run(intent: EffectIntent, error: ToolError, workspace: &Path) -> ToolObservation {
    if intent.call.mutates() {
        return uncertain(intent, workspace);
    }
    match error {
        ToolError::Cancelled => super::cancelled_read(intent),
        ToolError::ChangedInput => not_run(intent, ToolError::ChangedInput),
        error => {
            let (disposition, output, _, limit_cause) =
                super::dispatch::outcome(&intent, Err(error));
            observation(intent, disposition, output, None, limit_cause)
        }
    }
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
    git: &'a Git<'a>,
    deadline: tokio::time::Instant,
    hidden: &'a HiddenPaths,
    history_complete: bool,
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

impl BranchRead<'_> {
    async fn capture(
        &self,
        argv: &[String],
        deadline: tokio::time::Instant,
    ) -> Result<Captured, ToolError> {
        self.git.run(argv, Io::default(), deadline).await
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
                    return Ok(branch_result(
                        self.history_complete,
                        Vec::new(),
                        None,
                        true,
                        true,
                    ));
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
                self.history_complete,
                rows.rows,
                comparison,
                truncated,
                false,
            )),
            BranchCompletion::TimedOut => Ok(branch_result(
                self.history_complete,
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
        let listed = records(&list.stdout, b'\n', list.overflow)?;
        let mut truncated = list.overflow || listed.len() > max_count as usize;
        for record in listed.into_iter().take(max_count as usize) {
            if tokio::time::Instant::now() >= self.deadline {
                return Ok(BranchCompletion::TimedOut);
            }
            let mut row = branch_row(record, comparison.is_some())?;
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
            self.history_complete,
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
    history_complete: bool,
    branches: Vec<Value>,
    comparison_base: Option<Value>,
    truncated: bool,
    timed_out: bool,
) -> Value {
    json!({"branches": branches, "comparison_base": comparison_base,
        "history_complete": history_complete, "truncated": truncated, "timed_out": timed_out})
}

fn branch_row(raw: &[u8], comparison: bool) -> Result<Value, ToolError> {
    let fields: Vec<_> = raw.split(|byte| *byte == 0).collect();
    let [name, oid, head, symbolic, kind, upstream, track] = fields.as_slice() else {
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
    let (upstream, status) = if !local {
        (None, UpstreamStatus::NotApplicable)
    } else if !name_exact || std::str::from_utf8(upstream).is_err() {
        (None, UpstreamStatus::Unsupported)
    } else if upstream.is_empty() {
        (None, UpstreamStatus::Unavailable)
    } else {
        let target = String::from_utf8_lossy(upstream).into_owned();
        let status = if *track == b"gone" {
            UpstreamStatus::Gone
        } else {
            UpstreamStatus::Present
        };
        (Some(target), status)
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::test_support::process::BoundedOutput;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn none() -> HiddenPaths {
        HiddenPaths::default()
    }

    // The test process's own Git, resolved the way a Run resolves it.
    pub(super) fn git_program() -> super::super::config::Program {
        let root = tempfile::tempdir().unwrap();
        resolve(&environment(root.path(), root.path(), Path::new("/"))).expect("git on PATH")
    }

    // A Run's Git environment for `workspace`, with `home` as the user's home directory.
    pub(super) fn environment(workspace: &Path, editors: &Path, home: &Path) -> Environment {
        Environment::from_variables(workspace, editors, |name| match name {
            "HOME" => Some(home.into()),
            // Isolated child fixtures clear the environment; the platform directories still hold Git.
            "PATH" => Some(std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into())),
            _ => None,
        })
        .unwrap()
    }

    pub(super) fn host_git_output(
        repository: &Path,
        home: &Path,
        args: &[&str],
    ) -> std::process::Output {
        std::process::Command::new(git_program().executable)
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

    // Hostile markers the cage must prevent; the user's out-of-folder filter driver is allowed.
    pub(super) fn caged_markers(markers: &Path) -> Vec<String> {
        std::fs::read_dir(markers)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "clean")
            .collect()
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
        editors: &Path,
        call: &ToolCall,
        hidden: &HiddenPaths,
    ) -> Value {
        let program = git_program();
        let home = workspace.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let environment = environment(workspace, editors, &home);
        let git = Git::workspace(&program, &environment);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(execute(
                &git,
                call,
                hidden,
                16 * 1024,
                tokio::time::Instant::now() + Duration::from_secs(30),
            ))
            .expect("typed Git read");
        serde_json::from_str(&output).unwrap()
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
        assert_eq!(hostile["branches"][0]["upstream_status"], "present");
        assert_eq!(caged_markers(&markers), Vec::<String>::new());
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
        let name = format!("refs/heads/{}", "\u{00e9}".repeat(REF_BYTES));
        let raw = format!("{name}\0{}\0 \0\0commit\0\0", main["oid"].as_str().unwrap());
        let clipped = branch_row(raw.as_bytes(), false).unwrap();
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
        alias.extend_from_slice(b"refs/remotes/review/legacy-\xff\0commit\0\0");
        let alias = branch_row(&alias, false).unwrap();
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
        let output = std::process::Command::new(git_program().executable)
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

    #[tokio::test]
    async fn branch_deadline_returns_the_completed_prefix_without_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let program = git_program();
        let environment = environment(
            &workspace,
            &scratch(temp.path(), 510),
            &temp.path().join("home"),
        );
        let git = Git::workspace(&program, &environment);
        let hidden = none();
        let deadline = tokio::time::Instant::now() + MAX_GIT_RUNTIME;
        let first = execute(
            &git,
            &branches(None, None, Some(1)),
            &hidden,
            16 * 1024,
            deadline,
        )
        .await
        .unwrap();
        let first: Value = serde_json::from_str(&first).unwrap();
        let missing = super::super::config::Program {
            executable: "/no-dispatch".into(),
            ..program.clone()
        };
        let stopped = Git::workspace(&missing, &environment);
        let read = BranchRead {
            git: &stopped,
            deadline: tokio::time::Instant::now() - Duration::from_secs(1),
            hidden: &hidden,
            history_complete: true,
            budget: 16 * 1024,
        };
        let mut rows = Rows::new(read.budget);
        assert!(rows.push(first["branches"][0].clone()));
        let pending = captured(b"must not be parsed or dispatched\n", false);
        assert!(matches!(
            read.collect(&pending, 1, None, &mut rows).await.unwrap(),
            BranchCompletion::TimedOut
        ));
        let result = branch_result(true, rows.rows, None, true, true);
        assert_eq!(result["branches"], first["branches"]);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["truncated"], true);
        let expired = execute(
            &git,
            &branches(None, None, None),
            &hidden,
            16 * 1024,
            tokio::time::Instant::now() + BRANCH_CLEANUP_RESERVE,
        )
        .await
        .unwrap();
        let expired: Value = serde_json::from_str(&expired).unwrap();
        assert_eq!(expired["branches"], json!([]));
        assert_eq!(expired["timed_out"], true);
        assert_eq!(expired["truncated"], true);
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
    fn cage_keeps_hostile_repository_programs_from_running() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let markers = arm_hostile(temp.path(), &workspace);
        let before = tree(&workspace.join(".git"));

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
        // Configuration outside the folder is the user's: its filter driver may run, nothing else.
        assert_eq!(caged_markers(&markers), Vec::<String>::new());
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
        let status = run_call(
            &workspace,
            &scratch(temp.path(), 0),
            &ToolCall::GitStatus { path: ".".into() },
        );
        assert!(status.get("failed").is_none(), "{status}");
        assert_eq!(tree(&workspace.join(".git")), before);
        assert_eq!(caged_markers(&markers), Vec::<String>::new());
    }

    #[test]
    fn stale_stat_diff_changes_only_the_refreshed_index() {
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
        // Like `git diff` in a terminal, the read may refresh index stat data, never other state.
        let without_index = |tree: Vec<(PathBuf, Vec<u8>)>| {
            tree.into_iter()
                .filter(|(path, _)| path != Path::new("index"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            without_index(tree(&workspace.join(".git"))),
            without_index(before)
        );
        assert_eq!(caged_markers(&markers), Vec::<String>::new());
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

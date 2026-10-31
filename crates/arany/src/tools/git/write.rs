use super::super::ToolError;
use super::super::permissions::HiddenPaths;
use super::super::types::{GitHead, GitRef, GitRefs, ToolCall};
use super::runner::{Captured, Git, Io};
use super::{failure, owned, pathspecs_of, valid_branch};
use serde_json::json;

const DETACHED: &str =
    "HEAD is not on a local branch; create or switch to a branch before committing.";
const DETACHED_SWITCH: &str =
    "HEAD is detached; create a branch at it first so no commit is left unreachable, then switch.";
const DETACHED_HEAD: &str = "detached";

// A write's model-facing result, the refs it observed around the operation and its Git exit status.
pub(super) struct Written {
    pub(super) output: String,
    pub(super) refs: Option<Box<GitRefs>>,
    pub(super) exit_code: Option<i32>,
}

struct Write<'a> {
    git: &'a Git<'a>,
    hidden: &'a HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
}

impl Write<'_> {
    async fn run(&self, argv: &[&str]) -> Result<Captured, ToolError> {
        let mut full = owned(&["--no-pager"]);
        full.extend(owned(argv));
        self.git.run(&full, Io::default(), self.deadline).await
    }

    // A nonzero exit or overlong output becomes the model-facing failure.
    fn failed(&self, captured: &Captured) -> Option<String> {
        if captured.overflow {
            return Some(failure(
                "git output exceeded its bound",
                &[],
                self.max_bytes,
                self.hidden,
            ));
        }
        (captured.status != Some(0)).then(|| {
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
            failure(&message, detail, self.max_bytes, self.hidden)
        })
    }

    async fn oid(&self, name: &str) -> Result<Option<String>, ToolError> {
        let captured = self
            .run(&["rev-parse", "-q", "--verify", "--end-of-options", name])
            .await?;
        if captured.status != Some(0) {
            return Ok(None);
        }
        let oid = captured
            .stdout
            .strip_suffix(b"\n")
            .unwrap_or(&captured.stdout);
        super::object_id(oid).map(|oid| Some(oid.to_owned()))
    }

    async fn head(&self, target: Option<&str>) -> Result<GitHead, ToolError> {
        let symbolic = self.run(&["symbolic-ref", "-q", "HEAD"]).await?;
        let head = match symbolic.status {
            Some(0) => String::from_utf8(symbolic.stdout)
                .map_err(|_| ToolError::Operation)?
                .trim_end()
                .to_owned(),
            Some(1) => DETACHED_HEAD.to_owned(),
            _ => return Err(ToolError::Operation),
        };
        let target = match target {
            Some(name) => Some(GitRef {
                name: name.to_owned(),
                oid: self.oid(name).await?,
            }),
            None => None,
        };
        Ok(GitHead {
            oid: self.oid("HEAD").await?,
            head,
            target,
        })
    }
}

fn branch_of(head: &GitHead) -> Option<&str> {
    head.head
        .strip_prefix("refs/heads/")
        .filter(|name| valid_branch(name))
}

pub(super) async fn execute(
    git: &Git<'_>,
    call: &ToolCall,
    hidden: &HiddenPaths,
    max_bytes: usize,
    deadline: tokio::time::Instant,
) -> Result<Written, ToolError> {
    let write = Write {
        git,
        hidden,
        max_bytes,
        deadline,
    };
    let target = match call {
        ToolCall::GitBranch {
            name: Some(name), ..
        }
        | ToolCall::GitSwitch { branch: name } => Some(format!("refs/heads/{name}")),
        _ => None,
    };
    let before = write.head(target.as_deref()).await?;
    let (argv, before) = match call {
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => match write.commit_argv(before, message, paths, *staged).await? {
            Ok(prepared) => prepared,
            Err(written) => return Ok(written),
        },
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => {
            let mut argv = owned(&["branch", "--no-track", "--end-of-options", name]);
            argv.extend(start_point.iter().cloned());
            (argv, before)
        }
        ToolCall::GitSwitch { branch } => match write.switch_argv(before, branch) {
            Ok(prepared) => prepared,
            Err(written) => return Ok(written),
        },
        _ => return Err(ToolError::Configuration),
    };
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let captured = write.run(&argv).await?;
    let after = write.head(target.as_deref()).await?;
    let output = match (write.failed(&captured), call) {
        (Some(failed), _) => failed,
        (None, ToolCall::GitCommit { .. }) => {
            json!({"commit": after.oid, "branch": branch_of(&after)}).to_string()
        }
        (None, ToolCall::GitBranch { name, .. }) => json!({
            "branch": name,
            "oid": after.target.as_ref().and_then(|target| target.oid.clone()),
        })
        .to_string(),
        (None, _) => json!({"branch": branch_of(&after), "switched": true}).to_string(),
    };
    Ok(Written {
        output,
        refs: Some(Box::new(GitRefs { before, after })),
        exit_code: captured.status,
    })
}

// The write's argv and the head it started from, or the finished refusal or failed stage.
type Prepared = Result<(Vec<String>, GitHead), Written>;

impl Write<'_> {
    fn refuse(&self, message: &str, before: GitHead) -> Written {
        Written {
            output: failure(message, &[], self.max_bytes, self.hidden),
            refs: Some(Box::new(GitRefs {
                after: before.clone(),
                before,
            })),
            exit_code: None,
        }
    }

    async fn commit_argv(
        &self,
        before: GitHead,
        message: &str,
        paths: &[String],
        staged: bool,
    ) -> Result<Prepared, ToolError> {
        if branch_of(&before).is_none() {
            return Ok(Err(self.refuse(DETACHED, before)));
        }
        let message = format!("--message={message}");
        // "." already means every change; another path beside it must not narrow that.
        let specs = if paths.iter().any(|path| path == ".") {
            pathspecs_of(["."])
        } else {
            pathspecs_of(paths.iter().map(String::as_str))
        };
        let mut argv = vec!["commit".to_owned(), "--quiet".into(), message];
        if !staged {
            let mut add = vec!["add", "--all", "--"];
            add.extend(specs.iter().map(String::as_str));
            let added = self.run(&add).await?;
            if let Some(failed) = self.failed(&added) {
                let after = self.head(None).await?;
                return Ok(Err(Written {
                    output: failed,
                    refs: Some(Box::new(GitRefs { before, after })),
                    exit_code: added.status,
                }));
            }
            argv.push("--".into());
            argv.extend(specs);
        }
        Ok(Ok((argv, before)))
    }

    fn switch_argv(&self, before: GitHead, branch: &str) -> Prepared {
        if before
            .target
            .as_ref()
            .is_some_and(|target| target.oid.is_none())
        {
            return Err(self.refuse("No local branch has that name.", before));
        }
        // A commit reachable only from a detached HEAD would be left behind.
        let Some(current) = branch_of(&before) else {
            return Err(self.refuse(DETACHED_SWITCH, before));
        };
        if current == branch {
            return Err(Written {
                output: json!({"branch": branch, "switched": false}).to_string(),
                refs: Some(Box::new(GitRefs {
                    after: before.clone(),
                    before,
                })),
                exit_code: Some(0),
            });
        }
        Ok((owned(&["switch", "--quiet", "--no-guess", branch]), before))
    }
}

#[cfg(test)]
mod tests;

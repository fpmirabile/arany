use super::runner::{Environment, Git};
use super::{GIT_CAPTURE_BYTES, STDERR_BYTES, git_directory, read_optional, valid_branch};
use crate::tools::config::Config;
use crate::tools::types::{
    DEFAULT_PR_LIMIT, DEFAULT_PR_STATE, EffectIntent, HostIntent, MAX_PR_LIST_LIMIT, PrState,
    ToolCall, ToolDisposition, ToolObservation,
};
use crate::tools::{Attestation, CacheUpdate, Cancellation, ToolError, UnguardedReceipt};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const UNKNOWN_HOST_GIT_OUTCOME: &str =
    "host: remote outcome unknown. Inspect remote state; never retry automatically.";
pub(crate) const NON_FAST_FORWARD: &str = "host: remote reported no ref update (non-fast-forward).";
const REFUSED: &str = "host: destination, branch source, pinned executable or repository unavailable; nothing dispatched.";
const PREPARATION_TIMEOUT: &str =
    "host: preparation exceeded the runtime deadline; nothing dispatched.";
const CANCELLED: &str = "host: cancelled; nothing dispatched.";
pub(crate) const PR_LIST_FAILED: &str =
    "host: GitHub pull request list failed; no result was reported.";
pub(crate) const PR_LIST_CAPTURE_LIMIT: &str =
    "host: GitHub pull request list exceeded a capture limit.";
pub(crate) const PR_LIST_INVALID: &str = "host: GitHub returned invalid pull request data.";
pub(crate) const PR_VIEW_FAILED: &str =
    "host: GitHub pull request inspection failed; no result was reported.";
pub(crate) const PR_VIEW_CAPTURE_LIMIT: &str =
    "host: GitHub pull request inspection exceeded a capture limit.";
pub(crate) const PR_VIEW_INVALID: &str =
    "host: GitHub returned invalid pull request inspection data.";
pub(crate) const PR_VIEW_RESULT_BUDGET: &str =
    "host: pull request inspection did not fit the result budget; nothing was reported.";
const PR_VIEW_REFUSAL_BUDGET: &str = "host: refused; nothing dispatched.";
pub(crate) const PR_LIST_TITLE_BYTES: usize = 256;
pub(crate) const PR_LIST_TEXT_BYTES: usize = 2048;
const PR_LIST_REFUSAL_BUDGET: &str = "host: refused; nothing dispatched.";
const PR_VIEW_COMMENT_LIMIT: usize = 50;
const PR_VIEW_REVIEW_LIMIT: usize = 50;
const PR_VIEW_CHECK_LIMIT: usize = 100;
const PR_VIEW_TEXT_BYTES: usize = 8 * 1024;
const PR_VIEW_QUERY_BYTES: usize = 8 * 1024;
const MAX_PR_NUMBER: u32 = i32::MAX as u32;
const MAX_PR_REPOSITORY_BYTES: usize = 256;
const MAX_PACKED_REFS_BYTES: usize = 32 * 1024 * 1024;
const REWRITTEN: &str = "host: Git configuration rewrites this push URL (insteadOf or pushInsteadOf), so it differs from the approved destination; nothing dispatched.";
const PREPARATION_LIMIT: &str = "host: PR query over its 8 KiB bound; nothing dispatched.";

#[derive(Clone, Copy)]
enum PrViewConnection {
    Comments,
    Reviews,
    Checks,
}

impl PrViewConnection {
    const fn field(self) -> &'static str {
        match self {
            Self::Comments => "comments",
            Self::Reviews => "reviews",
            Self::Checks => "checks",
        }
    }

    const fn limit(self) -> usize {
        match self {
            Self::Comments => PR_VIEW_COMMENT_LIMIT,
            Self::Reviews => PR_VIEW_REVIEW_LIMIT,
            Self::Checks => PR_VIEW_CHECK_LIMIT,
        }
    }
}

pub(crate) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::GitPush { remote, branch } => {
            destination(remote).is_some() && valid_branch(branch)
        }
        ToolCall::GhPrCreate {
            repository,
            base,
            head,
            title,
            body,
            ..
        } => {
            repository_parts(repository).is_some()
                && valid_branch(base)
                && valid_branch(head)
                && base != head
                && text(title, 256, false)
                && !title.trim().is_empty()
                && text(body, 8192, true)
        }
        ToolCall::GhPrList {
            repository,
            base,
            head,
            limit,
            ..
        } => {
            repository.len() <= MAX_PR_REPOSITORY_BYTES
                && repository_parts(repository).is_some()
                && base.as_deref().is_none_or(valid_branch)
                && head.as_deref().is_none_or(valid_branch)
                && (1..=MAX_PR_LIST_LIMIT).contains(limit)
        }
        ToolCall::GhPrView {
            repository, number, ..
        } => {
            repository.len() <= MAX_PR_REPOSITORY_BYTES
                && repository_parts(repository).is_some()
                && (1..=MAX_PR_NUMBER).contains(number)
        }
        _ => false,
    }
}

fn text(value: &str, cap: usize, multiline: bool) -> bool {
    value.len() <= cap
        && !value
            .chars()
            .any(|c| c.is_control() && !(multiline && c == '\n'))
}

fn host_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

// Closed URL grammar prevents Git transport helpers, userinfo, redirects and option injection.
fn destination(value: &str) -> Option<(&str, bool)> {
    if value.len() > 2048 {
        return None;
    }
    let (rest, ssh) = if let Some(rest) = value.strip_prefix("https://") {
        (rest, false)
    } else {
        (value.strip_prefix("ssh://git@")?, true)
    };
    let (host, path) = rest.split_once('/')?;
    (host_name(host)
        && !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.starts_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        }))
    .then_some((host, ssh))
}

fn repository_parts(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.split('/');
    let (host, owner, repo) = (parts.next()?, parts.next()?, parts.next()?);
    (host_name(host)
        && parts.next().is_none()
        && [owner, repo].iter().all(|s| {
            !s.is_empty()
                && s.len() <= 100
                && !s.starts_with(['.', '-'])
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        }))
    .then_some((host, owner, repo))
}

fn valid_pr_view_url(url: &str, repository: &str, number: u32) -> bool {
    let Some((host, owner, name)) = repository_parts(repository) else {
        return false;
    };
    let Some(path) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((url_host, path)) = path.split_once('/') else {
        return false;
    };
    let mut segments = path.split('/');
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(url_owner), Some(url_name), Some("pull"), Some(url_number), None)
            if url_host == host
                && url_owner.eq_ignore_ascii_case(owner)
                && url_name.eq_ignore_ascii_case(name)
                && url_number == number.to_string()
    )
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    let string = || json!({"type":"string"});
    let mut pr_states: Vec<_> = PrState::ALL
        .into_iter()
        .map(|state| json!(state.as_str()))
        .collect();
    pr_states.push(Value::Null);
    Some(match operation {
        "git_push" => vec![
            (
                "remote",
                json!({"type":"string", "description":"Exact credential-free https://host/path or ssh://git@host/path URL, at most 2048 bytes. Repository remote names are not accepted."}),
            ),
            (
                "branch",
                json!({"type":"string", "description":"One existing local branch, pushed to the same explicit remote branch; at most 128 bytes."}),
            ),
        ],
        "gh_pr_create" => vec![
            (
                "repository",
                json!({"type":"string", "description":"Exact host/owner/repository; github.com unless explicitly configured otherwise."}),
            ),
            ("base", string()),
            ("head", string()),
            (
                "title",
                json!({"type":"string", "description":"Nonblank title, at most 256 bytes; no controls."}),
            ),
            (
                "body",
                json!({"type":"string", "description":"PR body, at most 8192 bytes; newlines permitted."}),
            ),
            ("draft", json!({"type":"boolean"})),
        ],
        "gh_pr_list" => vec![
            (
                "repository",
                json!({"type":"string", "description":format!("Exact host/owner/repository, at most {MAX_PR_REPOSITORY_BYTES} bytes.")}),
            ),
            (
                "state",
                json!({"type":["string","null"], "enum":pr_states, "description":format!("PR state; null defaults to {}.", DEFAULT_PR_STATE.as_str())}),
            ),
            (
                "base",
                json!({"type":["string","null"], "description":"Exact base branch name, at most 128 bytes, or null."}),
            ),
            (
                "head",
                json!({"type":["string","null"], "description":"Exact head branch name, at most 128 bytes, or null."}),
            ),
            (
                "limit",
                json!({"type":["integer","null"], "minimum":1, "maximum":MAX_PR_LIST_LIMIT, "description":format!("Maximum rows; null defaults to {DEFAULT_PR_LIMIT}.")}),
            ),
        ],
        "gh_pr_view" => vec![
            (
                "repository",
                json!({"type":"string", "description":format!("Exact host/owner/repository, at most {MAX_PR_REPOSITORY_BYTES} bytes.")}),
            ),
            (
                "number",
                json!({"type":"integer", "minimum":1, "maximum":MAX_PR_NUMBER}),
            ),
        ],
        _ => return None,
    })
}

pub(crate) fn valid_pin(call: &ToolCall, pin: &HostIntent) -> bool {
    let branch = match call {
        ToolCall::GitPush { branch, .. } => branch,
        ToolCall::GhPrCreate { head, .. } => head,
        ToolCall::GhPrList { .. } => {
            return pin.source_oid == "gh-pr-list-v1" && pin.refspec == "fixed-list";
        }
        ToolCall::GhPrView { .. } => {
            return pin.source_oid == "gh-pr-view-v1" && pin.refspec == "fixed-view-v1";
        }
        _ => return false,
    };
    pin.source_oid.len() == 40
        && pin
            .source_oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && pin.refspec == format!("{}:refs/heads/{branch}", pin.source_oid)
}

pub(crate) fn pin(workspace: &Path, call: &ToolCall) -> Result<HostIntent, ToolError> {
    if !valid(call) {
        return Err(ToolError::Configuration);
    }
    if matches!(call, ToolCall::GhPrList { .. } | ToolCall::GhPrView { .. }) {
        return Ok(HostIntent {
            source_oid: if matches!(call, ToolCall::GhPrList { .. }) {
                "gh-pr-list-v1"
            } else {
                "gh-pr-view-v1"
            }
            .into(),
            refspec: if matches!(call, ToolCall::GhPrList { .. }) {
                "fixed-list"
            } else {
                "fixed-view-v1"
            }
            .into(),
        });
    }
    pin_from_git(&git_directory(workspace)?, call)
}

fn pin_from_git(git: &Dir, call: &ToolCall) -> Result<HostIntent, ToolError> {
    if !valid(call) {
        return Err(ToolError::Configuration);
    }
    let branch = match call {
        ToolCall::GitPush { branch, .. } => branch,
        ToolCall::GhPrCreate { head, .. } => head,
        _ => return Err(ToolError::Configuration),
    };
    let name = format!("refs/heads/{branch}");
    let oid = match read_ref(git, &name)? {
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|_| ToolError::Path)?
            .trim_end_matches('\n')
            .to_owned(),
        None => {
            let packed =
                read_optional(git, "packed-refs", MAX_PACKED_REFS_BYTES)?.ok_or(ToolError::Path)?;
            std::str::from_utf8(&packed)
                .map_err(|_| ToolError::Path)?
                .lines()
                .find_map(|line| {
                    line.split_once(' ')
                        .filter(|(_, found)| *found == name)
                        .map(|(oid, _)| oid.to_owned())
                })
                .ok_or(ToolError::Path)?
        }
    };
    let pin = HostIntent {
        refspec: format!("{oid}:{name}"),
        source_oid: oid,
    };
    valid_pin(call, &pin).then_some(pin).ok_or(ToolError::Path)
}

fn read_ref(git: &Dir, name: &str) -> Result<Option<Vec<u8>>, ToolError> {
    let mut directory = git.try_clone().map_err(|_| ToolError::Path)?;
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return read_optional(&directory, part, 128);
        }
        match directory.open_dir_nofollow(part) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ToolError::Path),
        }
    }
    Err(ToolError::Path)
}

async fn capture(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ToolError::Operation)?;
    if bytes.len() > cap {
        return Err(ToolError::Limit);
    }
    Ok(bytes)
}

// Git pushes with the user's configuration, credential helpers and agent; the cage still applies.
async fn prepare_push(
    config: &Config,
    environment: &Environment,
    intent: &EffectIntent,
    deadline: tokio::time::Instant,
) -> Result<(Command, String), String> {
    let ToolCall::GitPush { remote, .. } = &intent.call else {
        return Err(REFUSED.into());
    };
    let program = config.git.as_ref().ok_or(REFUSED)?;
    let git = Git::workspace(program, environment);
    if let Some(refusal) = git.admit(deadline).await.map_err(|_| REFUSED)? {
        return Err(format!("host: {refusal}"));
    }
    if git.rewrites(remote, deadline).await.map_err(|_| REFUSED)? {
        return Err(REWRITTEN.into());
    }
    if Some(&pin(environment.workspace(), &intent.call).map_err(|_| REFUSED)?)
        != intent.host.as_ref()
    {
        return Err(REFUSED.into());
    }
    super::runner::verify(program).map_err(|_| REFUSED)?;
    let refspec = &intent.host.as_ref().ok_or(REFUSED)?.refspec;
    let argv = [
        "push",
        "--porcelain",
        "--no-follow-tags",
        "--recurse-submodules=no",
        "--",
        remote,
        refspec,
    ]
    .map(String::from);
    let mut command = git.command(&argv, true);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok((command, program.sha256.clone()))
}

// gh runs from its per-Run pin with the user's own gh configuration and authentication.
fn gh_command(config: &Config, environment: &Environment) -> Result<(Command, String), ToolError> {
    let gh = config.pinned_gh().ok_or(ToolError::Configuration)?;
    gh.verify()?;
    let mut command = environment.command(Path::new(&gh.executable));
    command
        .current_dir(environment.workspace())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok((command, gh.sha256.clone()))
}

fn gh_arguments(call: &ToolCall) -> Result<Vec<String>, ToolError> {
    let args = match call {
        ToolCall::GhPrCreate {
            repository,
            base,
            head,
            title,
            body,
            draft,
        } => {
            let mut args = vec![
                "pr".into(),
                "create".into(),
                "--repo".into(),
                repository.clone(),
                "--base".into(),
                base.clone(),
                "--head".into(),
                head.clone(),
                "--title".into(),
                title.clone(),
                "--body".into(),
                body.clone(),
            ];
            if *draft {
                args.push("--draft".into());
            }
            args
        }
        ToolCall::GhPrList {
            repository,
            state,
            base,
            head,
            limit,
        } => {
            let mut args = vec![
                "pr".into(),
                "list".into(),
                "--repo".into(),
                repository.clone(),
                "--state".into(),
                state.as_str().into(),
                "--limit".into(),
                limit.to_string(),
                "--json".into(),
                "number,title,url,author,state,isDraft,baseRefName,baseRefOid,headRefName,headRefOid,reviewDecision,updatedAt".into(),
            ];
            if let Some(base) = base {
                args.extend(["--base".into(), base.clone()]);
            }
            if let Some(head) = head {
                args.extend(["--head".into(), head.clone()]);
            }
            args
        }
        ToolCall::GhPrView { repository, number } => vec![
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            repository.clone(),
            "--json".into(),
            "number,title,url,author,state,isDraft,baseRefName,baseRefOid,headRefName,headRefOid,reviewDecision,updatedAt,body,mergeable".into(),
        ],
        _ => return Err(ToolError::Configuration),
    };
    Ok(args)
}

const PR_VIEW_QUERY_TEMPLATE: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid reviewDecision comments(last:__COMMENT_LIMIT__){pageInfo{hasPreviousPage} nodes{author{login} createdAt body}} reviews(last:__REVIEW_LIMIT__){pageInfo{hasPreviousPage} nodes{author{login} submittedAt state body}} commits(last:1){nodes{commit{statusCheckRollup{contexts(first:__CHECK_LIMIT__){pageInfo{hasNextPage} nodes{__typename ... on CheckRun{name status conclusion} ... on StatusContext{context state}}}}}}}}}}";

fn pr_view_query() -> String {
    PR_VIEW_QUERY_TEMPLATE
        .replace("__COMMENT_LIMIT__", &PR_VIEW_COMMENT_LIMIT.to_string())
        .replace("__REVIEW_LIMIT__", &PR_VIEW_REVIEW_LIMIT.to_string())
        .replace("__CHECK_LIMIT__", &PR_VIEW_CHECK_LIMIT.to_string())
}

fn valid_graphql_delimiters(query: &str) -> bool {
    let mut braces = 0_i32;
    let mut parentheses = 0_i32;
    for byte in query.bytes() {
        match byte {
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'(' => parentheses += 1,
            b')' => parentheses -= 1,
            _ => {}
        }
        if braces < 0 || parentheses < 0 {
            return false;
        }
    }
    braces == 0 && parentheses == 0
}

fn prepare_graphql_command(
    config: &Config,
    environment: &Environment,
    identity: PrViewIdentity,
) -> Result<PrViewEnvironment, ToolError> {
    let (host, owner, name) =
        repository_parts(&identity.repository).ok_or(ToolError::Configuration)?;
    let query = pr_view_query();
    if !valid_graphql_delimiters(&query) {
        return Err(ToolError::Configuration);
    }
    let input = serde_json::to_vec(&json!({
        "query": query,
        "variables": {"owner": owner, "name": name, "number": identity.number},
    }))
    .map_err(|_| ToolError::Configuration)?;
    if input.len() > PR_VIEW_QUERY_BYTES {
        return Err(ToolError::Limit);
    }
    let (mut command, _) = gh_command(config, environment)?;
    command
        .args([
            "api",
            "graphql",
            "--hostname",
            host,
            "--method",
            "POST",
            "--input",
            "-",
        ])
        .stdin(Stdio::piped());
    Ok(PrViewEnvironment { command, input })
}

async fn prepare(
    config: &Config,
    environment: &Environment,
    intent: &EffectIntent,
    deadline: tokio::time::Instant,
) -> Result<(Command, String, Option<PrViewEnvironment>), String> {
    if matches!(intent.call, ToolCall::GitPush { .. }) {
        let (command, sha256) = prepare_push(config, environment, intent, deadline).await?;
        return Ok((command, sha256, None));
    }
    let prepared = || {
        let (mut command, sha256) = gh_command(config, environment)?;
        command.args(gh_arguments(&intent.call)?);
        let details = match &intent.call {
            ToolCall::GhPrView { repository, number } => Some(prepare_graphql_command(
                config,
                environment,
                PrViewIdentity {
                    repository: repository.clone(),
                    number: *number,
                },
            )?),
            _ => None,
        };
        Ok::<_, ToolError>((command, sha256, details))
    };
    prepared().map_err(|error| match error {
        ToolError::Limit => PREPARATION_LIMIT.to_owned(),
        _ => REFUSED.to_owned(),
    })
}

pub(crate) async fn run(
    config: &Config,
    environment: Option<&Environment>,
    intent: EffectIntent,
    mut cancellation: Cancellation,
) -> ToolObservation {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.effective_timeout_ms()));
    let Some(environment) = environment else {
        return observation(intent, ToolDisposition::Denied, REFUSED, None);
    };
    let prepared = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(CANCELLED.to_owned()),
        result = tokio::time::timeout_at(deadline, prepare(config, environment, &intent, deadline)) =>
            result.unwrap_or_else(|_| Err(PREPARATION_TIMEOUT.to_owned())),
    };
    let (command, executable, details) = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => return observation(intent, ToolDisposition::Denied, &reason, None),
    };
    match details {
        Some(environment) => {
            supervise_pr_view(PrViewDispatch {
                deadline,
                command,
                environment,
                intent,
                executable,
                cancellation,
            })
            .await
        }
        None => supervise(deadline, command, intent, executable, cancellation).await,
    }
}

fn observation(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: &str,
    receipt: Option<UnguardedReceipt>,
) -> ToolObservation {
    let output = if disposition == ToolDisposition::Denied
        && intent.call.is_host_pr_read()
        && output.len() > intent.limits.result_bytes as usize
    {
        if matches!(intent.call, ToolCall::GhPrView { .. }) {
            PR_VIEW_REFUSAL_BUDGET
        } else {
            PR_LIST_REFUSAL_BUDGET
        }
    } else {
        output
    };
    ToolObservation {
        spill: None,
        intent,
        disposition,
        output: output.into(),
        attestation: receipt.map(Attestation::Host),
        cache_update: CacheUpdate::NotApplicable,
        enforcement_cause: None,
        limit_cause: None,
        approval_cause: None,
        guard_stage: None,
    }
}

async fn discard(mut pipe: impl tokio::io::AsyncRead + Unpin, cap: usize) -> Result<(), ToolError> {
    let mut buffer = [0u8; 4096];
    let mut total = 0usize;
    loop {
        let size = pipe
            .read(&mut buffer)
            .await
            .map_err(|_| ToolError::Operation)?;
        total += size;
        if total > cap {
            return Err(ToolError::OutputCeiling);
        }
        if size == 0 {
            return Ok(());
        }
    }
}

fn pr_list_result(bytes: &[u8], limit: u8, result_bytes: usize) -> Option<String> {
    if !(1..=MAX_PR_LIST_LIMIT).contains(&limit) {
        return None;
    }
    let result_bytes = result_bytes.min(crate::tools::types::MAX_TOOL_RESULT_BYTES);
    let rows: Value = serde_json::from_slice(bytes).ok()?;
    let rows = rows.as_array()?;
    if rows.len() > usize::from(limit) {
        return None;
    }
    let values = rows.iter().map(pr_list_row).collect::<Option<Vec<_>>>()?;
    let mut accepted = Vec::with_capacity(values.len());
    let mut truncated = false;
    for value in values {
        accepted.push(value);
        let encoded = serde_json::to_string(&json!({
            "pull_requests": accepted.clone(),
            "possibly_more": rows.len() == usize::from(limit),
            "truncated": truncated,
        }))
        .ok()?;
        if encoded.len() > result_bytes {
            accepted.pop();
            truncated = true;
            break;
        }
    }
    let output = serde_json::to_string(&json!({
        "pull_requests": accepted,
        "possibly_more": rows.len() == usize::from(limit) || truncated,
        "truncated": truncated,
    }))
    .ok()?;
    (output.len() <= result_bytes).then_some(output)
}

fn pr_list_row(row: &Value) -> Option<Value> {
    let number = row.get("number")?.as_u64()?;
    if number == 0 {
        return None;
    }
    let mut truncated_fields = Vec::new();
    let review_decision = row.get("reviewDecision")?;
    let review_decision = if review_decision.is_null() {
        Value::Null
    } else {
        Value::String(pr_list_text(
            review_decision,
            PR_LIST_TEXT_BYTES,
            "review_decision",
            &mut truncated_fields,
        )?)
    };
    Some(json!({
        "number": number,
        "title": pr_list_text(row.get("title")?, PR_LIST_TITLE_BYTES, "title", &mut truncated_fields)?,
        "url": pr_list_text(row.get("url")?, PR_LIST_TEXT_BYTES, "url", &mut truncated_fields)?,
        "author": pr_list_text(row.get("author")?.get("login")?, PR_LIST_TEXT_BYTES, "author", &mut truncated_fields)?,
        "state": pr_list_text(row.get("state")?, PR_LIST_TEXT_BYTES, "state", &mut truncated_fields)?,
        "draft": row.get("isDraft")?.as_bool()?,
        "base": pr_list_text(row.get("baseRefName")?, PR_LIST_TEXT_BYTES, "base", &mut truncated_fields)?,
        "base_oid": pr_list_text(row.get("baseRefOid")?, PR_LIST_TEXT_BYTES, "base_oid", &mut truncated_fields)?,
        "head": pr_list_text(row.get("headRefName")?, PR_LIST_TEXT_BYTES, "head", &mut truncated_fields)?,
        "head_oid": pr_list_text(row.get("headRefOid")?, PR_LIST_TEXT_BYTES, "head_oid", &mut truncated_fields)?,
        "review_decision": review_decision,
        "updated_at": pr_list_text(row.get("updatedAt")?, PR_LIST_TEXT_BYTES, "updated_at", &mut truncated_fields)?,
        "truncated_fields": truncated_fields,
    }))
}

fn pr_list_text(
    value: &Value,
    cap: usize,
    field: &str,
    truncated: &mut Vec<String>,
) -> Option<String> {
    let text = value.as_str()?;
    let end = text
        .char_indices()
        .take_while(|(index, character)| index + character.len_utf8() <= cap)
        .last()
        .map_or(0, |(index, character)| index + character.len_utf8());
    if end < text.len() && !truncated.iter().any(|name| name == field) {
        truncated.push(field.to_owned());
    }
    Some(text[..end].to_owned())
}

#[derive(Clone)]
pub(crate) struct PrViewIdentity {
    pub(crate) repository: String,
    pub(crate) number: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrViewResultError {
    Invalid,
    Budget,
}

impl PrViewResultError {
    const fn message(self) -> &'static str {
        match self {
            Self::Invalid => PR_VIEW_INVALID,
            Self::Budget => PR_VIEW_RESULT_BUDGET,
        }
    }
}

fn pr_view_result(
    scalar: &Value,
    graph_text: &str,
    identity: PrViewIdentity,
    result_bytes: usize,
) -> Result<String, PrViewResultError> {
    let invalid = PrViewResultError::Invalid;
    if scalar.get("number").and_then(Value::as_u64) != Some(u64::from(identity.number)) {
        return Err(invalid);
    }
    if !valid_pr_view_url(
        scalar.get("url").and_then(Value::as_str).ok_or(invalid)?,
        &identity.repository,
        identity.number,
    ) {
        return Err(invalid);
    }
    let graph: Value = serde_json::from_str(graph_text).map_err(|_| invalid)?;
    if let Some(errors) = graph.get("errors")
        && !errors.as_array().ok_or(invalid)?.is_empty()
    {
        return Err(invalid);
    }
    let pull = graph
        .pointer("/data/repository/pullRequest")
        .ok_or(invalid)?;
    if pull.is_null() {
        return Err(invalid);
    }
    let graph_head = pull
        .get("headRefOid")
        .and_then(Value::as_str)
        .ok_or(invalid)?;
    let scalar_head = scalar
        .get("headRefOid")
        .and_then(Value::as_str)
        .ok_or(invalid)?;
    if !valid_oid(graph_head) || !valid_oid(scalar_head) {
        return Err(invalid);
    }
    let mut truncated_fields = Vec::new();
    let result = pr_view_payload(scalar, pull, identity, graph_head, &mut truncated_fields)
        .ok_or(invalid)?;
    fit_pr_view_result(result, result_bytes).ok_or(PrViewResultError::Budget)
}

fn pr_view_payload(
    scalar: &Value,
    pull: &Value,
    identity: PrViewIdentity,
    graph_head: &str,
    truncated_fields: &mut Vec<String>,
) -> Option<Value> {
    let review_decision = scalar.get("reviewDecision")?;
    let review_decision = if review_decision.is_null() {
        Value::Null
    } else {
        Value::String(pr_view_text(
            review_decision,
            PR_LIST_TEXT_BYTES,
            "review_decision",
            truncated_fields,
        )?)
    };
    Some(json!({
        "pull_request": {
            "number": identity.number,
            "title": pr_view_text(scalar.get("title")?, PR_LIST_TITLE_BYTES, "title", truncated_fields)?,
            "url": pr_view_text(scalar.get("url")?, PR_LIST_TEXT_BYTES, "url", truncated_fields)?,
            "author": pr_view_nullable_text(scalar.get("author")?.get("login"), PR_LIST_TEXT_BYTES, "author", truncated_fields)?,
            "state": pr_view_text(scalar.get("state")?, PR_LIST_TEXT_BYTES, "state", truncated_fields)?,
            "draft": scalar.get("isDraft")?.as_bool()?,
            "base": pr_view_text(scalar.get("baseRefName")?, PR_LIST_TEXT_BYTES, "base", truncated_fields)?,
            "base_oid": pr_view_text(scalar.get("baseRefOid")?, PR_LIST_TEXT_BYTES, "base_oid", truncated_fields)?,
            "head": pr_view_text(scalar.get("headRefName")?, PR_LIST_TEXT_BYTES, "head", truncated_fields)?,
            "head_oid": scalar.get("headRefOid")?,
            "checks_head_oid": graph_head,
            "review_decision": review_decision,
            "discussion_review_decision": pr_view_nullable_text(pull.get("reviewDecision"), PR_LIST_TEXT_BYTES, "discussion_review_decision", truncated_fields)?,
            "updated_at": pr_view_text(scalar.get("updatedAt")?, PR_LIST_TEXT_BYTES, "updated_at", truncated_fields)?,
            "body": pr_view_text(scalar.get("body")?, PR_VIEW_TEXT_BYTES, "body", truncated_fields)?,
            "mergeable": pr_view_nullable_text(scalar.get("mergeable"), PR_LIST_TEXT_BYTES, "mergeable", truncated_fields)?,
            "comments": pr_view_comments(pull.get("comments")?, truncated_fields)?,
            "reviews": pr_view_reviews(pull.get("reviews")?, truncated_fields)?,
            "checks": pr_view_checks(pull, truncated_fields)?,
            "stale": !graph_head.eq_ignore_ascii_case(scalar.get("headRefOid")?.as_str()?),
            "truncated_fields": truncated_fields,
        }
    }))
}

fn fit_pr_view_result(mut result: Value, result_bytes: usize) -> Option<String> {
    let budget = result_bytes.min(crate::tools::types::MAX_TOOL_RESULT_BYTES);
    let output = serde_json::to_string(&result).ok()?;
    let mut bytes = output.len();
    if bytes <= budget {
        return Some(output);
    }
    for kind in [
        PrViewConnection::Checks,
        PrViewConnection::Reviews,
        PrViewConnection::Comments,
    ] {
        trim_pr_view_connection(
            &mut result["pull_request"][kind.field()],
            kind,
            budget,
            &mut bytes,
        )?;
    }
    loop {
        let output = serde_json::to_string(&result).ok()?;
        if output.len() <= budget {
            return Some(output);
        }
        let body = result["pull_request"]["body"].as_str()?;
        if body.is_empty() {
            return None;
        }
        let keep = body.len() / 2;
        let end = body
            .char_indices()
            .take_while(|(index, character)| index + character.len_utf8() <= keep)
            .last()
            .map_or(0, |(index, character)| index + character.len_utf8());
        result["pull_request"]["body"] = Value::String(body[..end].to_owned());
        let fields = result["pull_request"]["truncated_fields"].as_array_mut()?;
        if !fields.iter().any(|field| field == "body") {
            fields.push(Value::String("body".into()));
        }
    }
}

fn trim_pr_view_connection(
    connection: &mut Value,
    kind: PrViewConnection,
    budget: usize,
    bytes: &mut usize,
) -> Option<()> {
    if *bytes <= budget || connection.is_null() {
        return Some(());
    }
    let truncated = connection.get("truncated")?.as_bool()?;
    let items = connection.get_mut("items")?.as_array_mut()?;
    if items.is_empty() {
        return Some(());
    }
    let mut removed = 0;
    while removed < items.len() && (removed == 0 || *bytes > budget) {
        let index = match kind {
            PrViewConnection::Checks => items.len() - removed - 1,
            PrViewConnection::Comments | PrViewConnection::Reviews => removed,
        };
        let row_bytes = serde_json::to_vec(&items[index]).ok()?.len();
        let comma_bytes = usize::from(removed + 1 < items.len());
        *bytes = bytes.checked_sub(row_bytes + comma_bytes)?;
        if removed == 0 {
            // JSON true is one byte shorter than false.
            *bytes = bytes.checked_sub(usize::from(!truncated))?;
        }
        removed += 1;
    }
    match kind {
        PrViewConnection::Checks => items.truncate(items.len() - removed),
        PrViewConnection::Comments | PrViewConnection::Reviews => {
            items.drain(..removed);
        }
    }
    connection["truncated"] = Value::Bool(true);
    Some(())
}

fn pr_view_text(
    value: &Value,
    cap: usize,
    field: &str,
    truncated: &mut Vec<String>,
) -> Option<String> {
    pr_list_text(value, cap, field, truncated)
}

fn pr_view_comments(connection: &Value, truncated: &mut Vec<String>) -> Option<Value> {
    let nodes = connection.get("nodes")?.as_array()?;
    if nodes.len() > PR_VIEW_COMMENT_LIMIT {
        return None;
    }
    Some(json!({
        "items": nodes.iter().map(|comment| Some(json!({
            "author": pr_view_nullable_text(comment.get("author")?.get("login"), PR_LIST_TEXT_BYTES, "comment_author", truncated)?,
            "created_at": pr_view_text(comment.get("createdAt")?, PR_LIST_TEXT_BYTES, "comment_created_at", truncated)?,
            "body": pr_view_text(comment.get("body")?, PR_VIEW_TEXT_BYTES, "comment_body", truncated)?,
        }))).collect::<Option<Vec<_>>>()?,
        "truncated": connection.get("pageInfo")?.get("hasPreviousPage")?.as_bool()?,
    }))
}

fn pr_view_reviews(connection: &Value, truncated: &mut Vec<String>) -> Option<Value> {
    let nodes = connection.get("nodes")?.as_array()?;
    if nodes.len() > PR_VIEW_REVIEW_LIMIT {
        return None;
    }
    Some(json!({
        "items": nodes.iter().map(|review| Some(json!({
            "author": pr_view_nullable_text(review.get("author")?.get("login"), PR_LIST_TEXT_BYTES, "review_author", truncated)?,
            "submitted_at": pr_view_nullable_text(review.get("submittedAt"), PR_LIST_TEXT_BYTES, "review_submitted_at", truncated)?,
            "state": pr_view_text(review.get("state")?, PR_LIST_TEXT_BYTES, "review_state", truncated)?,
            "body": pr_view_text(review.get("body")?, PR_VIEW_TEXT_BYTES, "review_body", truncated)?,
        }))).collect::<Option<Vec<_>>>()?,
        "truncated": connection.get("pageInfo")?.get("hasPreviousPage")?.as_bool()?,
    }))
}

fn pr_view_nullable_text(
    value: Option<&Value>,
    cap: usize,
    field: &str,
    truncated: &mut Vec<String>,
) -> Option<Value> {
    match value {
        None | Some(Value::Null) => Some(Value::Null),
        Some(value) => Some(Value::String(pr_view_text(value, cap, field, truncated)?)),
    }
}

fn pr_view_checks(pull: &Value, truncated: &mut Vec<String>) -> Option<Value> {
    let Some(commit) = pull
        .get("commits")?
        .get("nodes")?
        .as_array()?
        .first()
        .and_then(|node| node.get("commit"))
    else {
        return Some(Value::Null);
    };
    let rollup = commit
        .get("statusCheckRollup")
        .filter(|value| !value.is_null());
    let Some(rollup) = rollup else {
        return Some(Value::Null);
    };
    let connection = rollup.get("contexts")?;
    let nodes = connection.get("nodes")?.as_array()?;
    if nodes.len() > PR_VIEW_CHECK_LIMIT {
        return None;
    }
    Some(json!({
        "items": nodes.iter().map(|check| {
            let typename = check.get("__typename")?.as_str()?;
            let (name, status, conclusion) = match typename {
                "CheckRun" => (check.get("name")?, check.get("status")?, Some(check.get("conclusion")?)),
                "StatusContext" => (check.get("context")?, check.get("state")?, None),
                _ => return None,
            };
            Some(json!({
                "name": pr_view_text(name, PR_LIST_TEXT_BYTES, "check_name", truncated)?,
                "status": pr_view_nullable_text(Some(status), PR_LIST_TEXT_BYTES, "check_status", truncated)?,
                "conclusion": pr_view_nullable_text(conclusion, PR_LIST_TEXT_BYTES, "check_conclusion", truncated)?,
            }))
        }).collect::<Option<Vec<_>>>()?,
        "truncated": connection.get("pageInfo")?.get("hasNextPage")?.as_bool()?,
    }))
}

pub(crate) fn valid_pr_list_output(output: &str, limit: u8) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.len() != 3
        || !object.contains_key("pull_requests")
        || !object.get("possibly_more").is_some_and(Value::is_boolean)
        || !object.get("truncated").is_some_and(Value::is_boolean)
    {
        return false;
    }
    let Some(rows) = object.get("pull_requests").and_then(Value::as_array) else {
        return false;
    };
    rows.len() <= usize::from(limit) && rows.iter().all(valid_pr_list_row)
}

pub(crate) fn valid_pr_view_output(output: &str, identity: PrViewIdentity) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    let Some(root) = value.as_object() else {
        return false;
    };
    let Some(pull) = value.get("pull_request").and_then(Value::as_object) else {
        return false;
    };
    root.len() == 1
        && pull.len() == 21
        && valid_pr_view_identity(pull, identity)
        && valid_pr_view_text_fields(pull)
        && valid_pr_view_status_fields(pull)
        && valid_pr_view_connections(pull)
        && valid_pr_view_truncated_fields(pull)
}

fn valid_pr_view_identity(pull: &serde_json::Map<String, Value>, identity: PrViewIdentity) -> bool {
    let number = pull.get("number").and_then(Value::as_u64);
    let url = pull.get("url").and_then(Value::as_str);
    let head = pull.get("head_oid").and_then(Value::as_str);
    let checks_head = pull.get("checks_head_oid").and_then(Value::as_str);
    number == Some(u64::from(identity.number))
        && url.is_some_and(|url| valid_pr_view_url(url, &identity.repository, identity.number))
        && head.zip(checks_head).is_some_and(|(head, checks_head)| {
            valid_oid(head)
                && valid_oid(checks_head)
                && pull.get("stale").and_then(Value::as_bool)
                    == Some(!head.eq_ignore_ascii_case(checks_head))
        })
}

fn valid_pr_view_text_fields(pull: &serde_json::Map<String, Value>) -> bool {
    [
        ("title", PR_LIST_TITLE_BYTES),
        ("state", PR_LIST_TEXT_BYTES),
        ("base", PR_LIST_TEXT_BYTES),
        ("base_oid", PR_LIST_TEXT_BYTES),
        ("head", PR_LIST_TEXT_BYTES),
        ("updated_at", PR_LIST_TEXT_BYTES),
        ("body", PR_VIEW_TEXT_BYTES),
    ]
    .iter()
    .all(|(field, cap)| {
        pull.get(*field)
            .and_then(Value::as_str)
            .is_some_and(|text| text.len() <= *cap)
    }) && ["author", "mergeable"].iter().all(|field| {
        pull.get(*field).is_some_and(|value| {
            value.is_null()
                || value
                    .as_str()
                    .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
        })
    })
}

fn valid_pr_view_status_fields(pull: &serde_json::Map<String, Value>) -> bool {
    pull.get("draft").is_some_and(Value::is_boolean)
        && ["review_decision", "discussion_review_decision"]
            .iter()
            .all(|field| {
                pull.get(*field).is_some_and(|value| {
                    value.is_null()
                        || value
                            .as_str()
                            .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                })
            })
}

fn valid_pr_view_connections(pull: &serde_json::Map<String, Value>) -> bool {
    pull.get("comments")
        .is_some_and(|value| valid_pr_view_connection(value, PrViewConnection::Comments))
        && pull
            .get("reviews")
            .is_some_and(|value| valid_pr_view_connection(value, PrViewConnection::Reviews))
        && (pull.get("checks").is_some_and(Value::is_null)
            || pull
                .get("checks")
                .is_some_and(|value| valid_pr_view_connection(value, PrViewConnection::Checks)))
}

fn valid_pr_view_truncated_fields(pull: &serde_json::Map<String, Value>) -> bool {
    pull.get("truncated_fields")
        .and_then(Value::as_array)
        .is_some_and(|fields| {
            let mut unique = std::collections::HashSet::new();
            fields.iter().all(|field| {
                field.as_str().is_some_and(|field| {
                    unique.insert(field)
                        && [
                            "title",
                            "url",
                            "author",
                            "state",
                            "base",
                            "base_oid",
                            "head",
                            "updated_at",
                            "review_decision",
                            "discussion_review_decision",
                            "body",
                            "mergeable",
                            "comment_author",
                            "comment_created_at",
                            "comment_body",
                            "review_author",
                            "review_submitted_at",
                            "review_state",
                            "review_body",
                            "check_name",
                            "check_status",
                            "check_conclusion",
                        ]
                        .contains(&field)
                })
            })
        })
}

fn valid_pr_view_connection(value: &Value, kind: PrViewConnection) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(items) = object.get("items").and_then(Value::as_array) else {
        return false;
    };
    object.len() == 2
        && object.contains_key("truncated")
        && object.get("truncated").is_some_and(Value::is_boolean)
        && items.len() <= kind.limit()
        && items.iter().all(|item| {
            let Some(item) = item.as_object() else {
                return false;
            };
            match kind {
                PrViewConnection::Comments => {
                    item.len() == 3
                        && item.get("author").is_some_and(|value| {
                            value.is_null()
                                || value
                                    .as_str()
                                    .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                        })
                        && item
                            .get("created_at")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                        && item
                            .get("body")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= PR_VIEW_TEXT_BYTES)
                }
                PrViewConnection::Reviews => {
                    item.len() == 4
                        && ["author", "submitted_at"].iter().all(|field| {
                            item.get(*field).is_some_and(|value| {
                                value.is_null()
                                    || value
                                        .as_str()
                                        .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                            })
                        })
                        && item
                            .get("state")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                        && item
                            .get("body")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= PR_VIEW_TEXT_BYTES)
                }
                PrViewConnection::Checks => {
                    item.len() == 3
                        && item
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                        && ["status", "conclusion"].iter().all(|field| {
                            item.get(*field).is_some_and(|value| {
                                value.is_null()
                                    || value
                                        .as_str()
                                        .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
                            })
                        })
                }
            }
        })
}

fn valid_pr_list_row(value: &Value) -> bool {
    let Some(row) = value.as_object() else {
        return false;
    };
    let fields = [
        "number",
        "title",
        "url",
        "author",
        "state",
        "draft",
        "base",
        "base_oid",
        "head",
        "head_oid",
        "review_decision",
        "updated_at",
        "truncated_fields",
    ];
    row.len() == fields.len()
        && fields.iter().all(|field| row.contains_key(*field))
        && row
            .get("number")
            .and_then(Value::as_u64)
            .is_some_and(|number| number > 0)
        && [
            ("title", PR_LIST_TITLE_BYTES),
            ("url", PR_LIST_TEXT_BYTES),
            ("author", PR_LIST_TEXT_BYTES),
            ("state", PR_LIST_TEXT_BYTES),
            ("base", PR_LIST_TEXT_BYTES),
            ("base_oid", PR_LIST_TEXT_BYTES),
            ("head", PR_LIST_TEXT_BYTES),
            ("head_oid", PR_LIST_TEXT_BYTES),
            ("updated_at", PR_LIST_TEXT_BYTES),
        ]
        .iter()
        .all(|(field, cap)| {
            row.get(*field)
                .and_then(Value::as_str)
                .is_some_and(|text| text.len() <= *cap)
        })
        && row.get("draft").is_some_and(Value::is_boolean)
        && row.get("review_decision").is_some_and(|value| {
            value.is_null()
                || value
                    .as_str()
                    .is_some_and(|text| text.len() <= PR_LIST_TEXT_BYTES)
        })
        && row
            .get("truncated_fields")
            .and_then(Value::as_array)
            .is_some_and(|fields| {
                fields.iter().all(|field| {
                    field.as_str().is_some_and(|field| {
                        [
                            "review_decision",
                            "title",
                            "url",
                            "author",
                            "state",
                            "base",
                            "base_oid",
                            "head",
                            "head_oid",
                            "updated_at",
                        ]
                        .contains(&field)
                    })
                })
            })
}

fn non_fast_forward(intent: &EffectIntent, output: &[u8]) -> bool {
    let ToolCall::GitPush { .. } = intent.call else {
        return false;
    };
    let Some(pin) = &intent.host else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let mut rows = text.lines().filter(|line| line.contains('\t'));
    let Some(row) = rows.next() else { return false };
    rows.next().is_none()
        && ["[rejected] (non-fast-forward)", "[rejected] (fetch first)"]
            .iter()
            .any(|reason| row == format!("!\t{}\t{reason}", pin.refspec))
}

async fn supervise(
    deadline: tokio::time::Instant,
    command: Command,
    intent: EffectIntent,
    executable_sha256: String,
    cancellation: Cancellation,
) -> ToolObservation {
    supervise_input(ProcessDispatch {
        deadline,
        command,
        intent,
        executable_sha256,
        cancellation,
        input: None,
    })
    .await
}

#[derive(Debug)]
struct PrViewEnvironment {
    command: Command,
    input: Vec<u8>,
}

struct PrViewDispatch {
    deadline: tokio::time::Instant,
    command: Command,
    environment: PrViewEnvironment,
    intent: EffectIntent,
    executable: String,
    cancellation: Cancellation,
}

async fn supervise_pr_view(dispatch: PrViewDispatch) -> ToolObservation {
    let PrViewDispatch {
        deadline,
        command,
        environment,
        intent,
        executable,
        cancellation,
    } = dispatch;
    let scalar = supervise(
        deadline,
        command,
        intent.clone(),
        executable.clone(),
        cancellation.clone(),
    )
    .await;
    if scalar.disposition != ToolDisposition::Succeeded {
        return scalar;
    }
    let identity = match &intent.call {
        ToolCall::GhPrView {
            repository, number, ..
        } => PrViewIdentity {
            repository: repository.clone(),
            number: *number,
        },
        _ => return scalar,
    };
    let scalar_json: Value = match serde_json::from_str(&scalar.output) {
        Ok(value) => value,
        Err(_) => return pr_view_failure(intent, scalar, PR_VIEW_INVALID),
    };
    if !scalar_json
        .get("headRefOid")
        .and_then(Value::as_str)
        .is_some_and(valid_oid)
    {
        return pr_view_failure(intent, scalar, PR_VIEW_INVALID);
    }
    supervise_pr_view_details(PrViewDetails {
        deadline,
        environment,
        executable,
        intent,
        scalar,
        scalar_json,
        identity,
        cancellation,
    })
    .await
}

struct PrViewDetails {
    deadline: tokio::time::Instant,
    environment: PrViewEnvironment,
    executable: String,
    intent: EffectIntent,
    scalar: ToolObservation,
    scalar_json: Value,
    identity: PrViewIdentity,
    cancellation: Cancellation,
}

async fn supervise_pr_view_details(details: PrViewDetails) -> ToolObservation {
    let PrViewDetails {
        deadline,
        environment,
        executable,
        intent,
        scalar,
        scalar_json,
        identity,
        cancellation,
    } = details;
    let PrViewEnvironment { command, input } = environment;
    let query = supervise_input(ProcessDispatch {
        deadline,
        command,
        intent: intent.clone(),
        executable_sha256: executable,
        cancellation,
        input: Some(input),
    })
    .await;
    if query.disposition != ToolDisposition::Succeeded {
        if query.attestation.is_none() {
            return pr_view_failure(intent, scalar, PR_VIEW_FAILED);
        }
        return query;
    }
    match pr_view_result(
        &scalar_json,
        &query.output,
        identity,
        intent.limits.result_bytes as usize,
    ) {
        Ok(output) => observation(
            intent,
            ToolDisposition::Succeeded,
            &output,
            query.attestation.and_then(host_receipt),
        ),
        Err(error) => observation(
            intent,
            ToolDisposition::Failed,
            error.message(),
            query.attestation.and_then(host_receipt),
        ),
    }
}

fn pr_view_failure(
    intent: EffectIntent,
    scalar: ToolObservation,
    output: &'static str,
) -> ToolObservation {
    observation(
        intent,
        ToolDisposition::Failed,
        output,
        scalar.attestation.and_then(host_receipt),
    )
}

fn host_receipt(attestation: Attestation) -> Option<UnguardedReceipt> {
    match attestation {
        Attestation::Host(receipt) => Some(receipt),
        _ => None,
    }
}

fn valid_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct ProcessDispatch {
    deadline: tokio::time::Instant,
    command: Command,
    intent: EffectIntent,
    executable_sha256: String,
    cancellation: Cancellation,
    input: Option<Vec<u8>>,
}

async fn write_process_input(
    mut stdin: impl tokio::io::AsyncWrite + Unpin,
    input: &[u8],
) -> Result<(), ToolError> {
    use tokio::io::AsyncWriteExt;

    let result = async {
        stdin.write_all(input).await?;
        stdin.shutdown().await
    }
    .await;
    // The reaped exit status decides an early reader close.
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(_) => Err(ToolError::Operation),
    }
}

async fn supervise_input(dispatch: ProcessDispatch) -> ToolObservation {
    let ProcessDispatch {
        deadline,
        mut command,
        intent,
        executable_sha256,
        mut cancellation,
        input,
    } = dispatch;
    if cancellation.is_cancelled() {
        return observation(intent, ToolDisposition::Denied, CANCELLED, None);
    }
    if tokio::time::Instant::now() >= deadline {
        return observation(intent, ToolDisposition::Denied, PREPARATION_TIMEOUT, None);
    }
    let Ok(mut child) = command.spawn() else {
        return observation(intent, ToolDisposition::Denied, REFUSED, None);
    };
    let pid = child.id().unwrap_or(0);
    let group = i32::try_from(pid).ok().and_then(Pid::from_raw);
    let completed = if let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) {
        let stdin = input.as_ref().and_then(|_| child.stdin.take());
        let write_input = async {
            if let Some(input) = input {
                write_process_input(stdin.ok_or(ToolError::Operation)?, &input).await?;
            }
            Ok::<_, ToolError>(())
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => None,
            result = tokio::time::timeout_at(deadline, async {
                tokio::try_join!(
                    async { crate::tools::guard::reap_owned_child(&mut child).await.map_err(|_| ToolError::Operation) },
                    capture(out, GIT_CAPTURE_BYTES), discard(err, STDERR_BYTES), write_input
                )
            }) => result.ok().map(|result| result.map(|(status, bytes, (), ())| (status, bytes))),
        }
    } else {
        None
    };
    let reaped = if completed
        .as_ref()
        .is_some_and(|result| result.as_ref().is_ok())
    {
        true
    } else {
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                crate::tools::guard::reap_owned_child(&mut child),
            )
            .await,
            Ok(Ok(_))
        )
    };
    let stopped = match group {
        Some(group) => crate::tools::unguarded::stop_group(group).await,
        None => false,
    };
    let outcome = classify_outcome(&intent, &completed, reaped, stopped);
    let receipt = UnguardedReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        executable_sha256,
        pid,
        exit_code: outcome.exit_code,
        descendants_confirmed_stopped: outcome.descendants_confirmed_stopped,
    };
    observation(intent, outcome.disposition, &outcome.output, Some(receipt))
}

struct HostOutcome {
    disposition: ToolDisposition,
    output: String,
    exit_code: Option<i32>,
    descendants_confirmed_stopped: bool,
}

type CapturedProcessResult = Result<(std::process::ExitStatus, Vec<u8>), ToolError>;

fn classify_outcome(
    intent: &EffectIntent,
    completed: &Option<CapturedProcessResult>,
    reaped: bool,
    stopped: bool,
) -> HostOutcome {
    let execution_status = completed
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|(status, _)| *status);
    let pr_list_capture_limited = matches!(
        completed,
        Some(Err(ToolError::Limit | ToolError::OutputCeiling))
    ) && intent.call.is_host_pr_read();
    let succeeded = reaped && stopped && execution_status.is_some_and(|status| status.success());
    let pr_read_confirmed_failure = intent.call.is_host_pr_read()
        && reaped
        && stopped
        && (pr_list_capture_limited || execution_status.is_some_and(|status| !status.success()));
    let rejected = reaped
        && stopped
        && execution_status.is_some_and(|status| status.code() == Some(1))
        && completed.as_ref().is_some_and(|result| {
            result
                .as_ref()
                .is_ok_and(|(_, bytes)| non_fast_forward(intent, bytes))
        });
    let output = if pr_list_capture_limited && reaped && stopped {
        if matches!(intent.call, ToolCall::GhPrView { .. }) {
            PR_VIEW_CAPTURE_LIMIT.into()
        } else {
            PR_LIST_CAPTURE_LIMIT.into()
        }
    } else if succeeded {
        match &intent.call {
            ToolCall::GhPrList { limit, .. } => completed
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|(_, bytes)| {
                    pr_list_result(bytes, *limit, intent.limits.result_bytes as usize)
                })
                .unwrap_or_else(|| PR_LIST_INVALID.into()),
            ToolCall::GhPrView { .. } => completed
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|(_, bytes)| String::from_utf8(bytes.clone()).ok())
                .unwrap_or_else(|| PR_VIEW_INVALID.into()),
            _ => "host: remote operation completed.".into(),
        }
    } else if pr_read_confirmed_failure {
        if matches!(intent.call, ToolCall::GhPrView { .. }) {
            PR_VIEW_FAILED.into()
        } else {
            PR_LIST_FAILED.into()
        }
    } else if rejected {
        NON_FAST_FORWARD.into()
    } else {
        UNKNOWN_HOST_GIT_OUTCOME.into()
    };
    let disposition = if pr_list_capture_limited && reaped && stopped {
        ToolDisposition::Failed
    } else if succeeded {
        if matches!(intent.call, ToolCall::GhPrList { .. }) && output == PR_LIST_INVALID
            || matches!(intent.call, ToolCall::GhPrView { .. }) && output == PR_VIEW_INVALID
        {
            ToolDisposition::Failed
        } else {
            ToolDisposition::Succeeded
        }
    } else if pr_read_confirmed_failure || rejected {
        ToolDisposition::Failed
    } else {
        ToolDisposition::Uncertain
    };
    HostOutcome {
        disposition,
        output,
        exit_code: execution_status.and_then(|status| status.code()),
        descendants_confirmed_stopped: reaped && stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git::tests::{arm_hostile, fixture, host_git, host_git_output};
    use crate::tools::types::{NetworkGrant, ToolLimits};
    use crate::validate::hex_digest;

    async fn supervise(
        command: Command,
        intent: EffectIntent,
        executable: String,
        cancellation: Cancellation,
    ) -> ToolObservation {
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.effective_timeout_ms()));
        super::supervise(deadline, command, intent, executable, cancellation).await
    }

    fn push() -> ToolCall {
        ToolCall::GitPush {
            remote: "https://github.com/example/project.git".into(),
            branch: "main".into(),
        }
    }

    fn pr() -> ToolCall {
        ToolCall::GhPrCreate {
            repository: "github.com/example/project".into(),
            base: "base".into(),
            head: "main".into(),
            title: "A title".into(),
            body: "First line\nSecond line".into(),
            draft: true,
        }
    }

    fn pr_list() -> ToolCall {
        ToolCall::GhPrList {
            repository: "github.com/example/project".into(),
            state: crate::tools::types::DEFAULT_PR_STATE,
            base: None,
            head: None,
            limit: DEFAULT_PR_LIMIT,
        }
    }

    fn pr_view_scalar(head_oid: &str) -> Value {
        json!({
            "number": 17, "title": "Review", "url": "https://github.com/example/project/pull/17",
            "author": {"login":"author"}, "state":"OPEN", "isDraft":false,
            "baseRefName":"main", "baseRefOid":"a".repeat(40), "headRefName":"topic",
            "headRefOid":head_oid, "reviewDecision":null, "updatedAt":"2026-10-22T00:00:00Z",
            "body":"Description", "mergeable":"MERGEABLE"
        })
    }

    fn pr_view_identity() -> PrViewIdentity {
        PrViewIdentity {
            repository: "github.com/example/project".into(),
            number: 17,
        }
    }

    fn pr_view_graph(head_oid: &str, check_status: &str) -> Value {
        json!({"data":{"repository":{"pullRequest":{
            "headRefOid":head_oid,
            "reviewDecision":"REVIEW_REQUIRED",
            "comments":{"pageInfo":{"hasPreviousPage":true},"nodes":[{"author":{"login":"commenter"},"createdAt":"2026-10-22T00:00:00Z","body":"Please update"}]},
            "reviews":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"author":{"login":"reviewer"},"submittedAt":"2026-10-22T00:00:00Z","state":"CHANGES_REQUESTED","body":"Needs work"}]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":check_status,"conclusion":null}]}}}}]}
        }}}})
    }

    #[tokio::test]
    async fn host_gh_pr_view_query_is_fixed_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let intent = intent(
            temp.path(),
            ToolCall::GhPrView {
                repository: "github.com/example/project".into(),
                number: 17,
            },
        );
        let (scalar, _, environment) = prepare_for(temp.path(), &intent).await.unwrap();
        let PrViewEnvironment { command, input } = environment.unwrap();
        assert_eq!(
            scalar.as_std().get_program(),
            command.as_std().get_program()
        );
        assert_eq!(
            scalar.as_std().get_envs().collect::<Vec<_>>(),
            command.as_std().get_envs().collect::<Vec<_>>()
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "api",
                "graphql",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "--input",
                "-"
            ]
        );
        let input: Value = serde_json::from_slice(&input).unwrap();
        assert_eq!(
            input["variables"],
            json!({"owner":"example","name":"project","number":17})
        );
        assert!(
            input["query"]
                .as_str()
                .unwrap()
                .contains(&format!("comments(last:{PR_VIEW_COMMENT_LIMIT})"))
        );
        assert!(
            input["query"]
                .as_str()
                .unwrap()
                .contains(&format!("reviews(last:{PR_VIEW_REVIEW_LIMIT})"))
        );
        assert!(
            input["query"]
                .as_str()
                .unwrap()
                .contains(&format!("contexts(first:{PR_VIEW_CHECK_LIMIT})"))
        );
        assert!(!input["query"].as_str().unwrap().contains("paginate"));
        assert!(valid_graphql_delimiters(input["query"].as_str().unwrap()));
        assert!(!valid_graphql_delimiters("query { repository("));
        assert!(serde_json::to_vec(&input).unwrap().len() <= PR_VIEW_QUERY_BYTES);
    }

    #[test]
    fn host_gh_pr_view_accepts_mixed_case_repository_identity() {
        let scalar_head = "a".repeat(40);
        let mut scalar = pr_view_scalar(&scalar_head);
        scalar["url"] = json!("https://github.com/Example/Project/pull/17");
        let output = pr_view_result(
            &scalar,
            &serde_json::to_string(&pr_view_graph(&scalar_head, "COMPLETED")).unwrap(),
            pr_view_identity(),
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        assert!(valid_pr_view_output(&output, pr_view_identity()));
    }

    #[test]
    fn host_gh_pr_view_budget_retains_recent_discussion_and_first_checks() {
        let head = "a".repeat(40);
        for (connection, field) in [
            ("comments", "body"),
            ("reviews", "body"),
            ("checks", "name"),
        ] {
            let mut graph = pr_view_graph(&head, "COMPLETED");
            let pull = &mut graph["data"]["repository"]["pullRequest"];
            let check_path = "/commits/nodes/0/commit/statusCheckRollup/contexts/nodes";
            let mut rows = if connection == "checks" {
                pull.pointer(check_path).unwrap().clone()
            } else {
                pull[connection]["nodes"].clone()
            };
            let template = rows[0].clone();
            rows = json!([template.clone(), template.clone(), template]);
            for (row, text) in rows.as_array_mut().unwrap().iter_mut().zip([
                "Earlier \"quoted\"",
                "Middle\\path",
                "Current 🧪\n",
            ]) {
                row[field] = json!(text);
            }
            for name in ["comments", "reviews"] {
                pull[name]["nodes"] = json!([]);
            }
            *pull.pointer_mut(check_path).unwrap() = json!([]);
            if connection == "checks" {
                *pull.pointer_mut(check_path).unwrap() = rows;
            } else {
                pull[connection]["nodes"] = rows;
            }
            let scalar = pr_view_scalar(&head);
            let output = pr_view_result(
                &scalar,
                &graph.to_string(),
                pr_view_identity(),
                crate::tools::types::MAX_TOOL_RESULT_BYTES,
            )
            .unwrap();
            let full: Value = serde_json::from_str(&output).unwrap();
            let items = full["pull_request"][connection]["items"]
                .as_array()
                .unwrap();
            for keep in 0..=items.len() {
                let mut expected = full.clone();
                let retained = if connection == "checks" {
                    &items[..keep]
                } else {
                    &items[items.len() - keep..]
                };
                expected["pull_request"][connection]["items"] = json!(retained);
                if keep < items.len() {
                    expected["pull_request"][connection]["truncated"] = json!(true);
                }
                let expected = serde_json::to_string(&expected).unwrap();
                let output = pr_view_result(
                    &scalar,
                    &graph.to_string(),
                    pr_view_identity(),
                    expected.len(),
                )
                .unwrap();
                assert!(valid_pr_view_output(&output, pr_view_identity()));
                assert_eq!(output, expected, "{connection}: retain {keep}");
            }
        }
    }

    #[test]
    fn host_gh_pr_view_preserves_pending_status_partiality_and_head_races() {
        let scalar_head = "a".repeat(40);
        let graph_head = "b".repeat(40);
        let result = pr_view_result(
            &pr_view_scalar(&scalar_head),
            &serde_json::to_string(&pr_view_graph(&graph_head, "IN_PROGRESS")).unwrap(),
            pr_view_identity(),
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert!(value["pull_request"]["stale"].as_bool().unwrap());
        assert_eq!(value["pull_request"]["head_oid"], scalar_head);
        assert_eq!(value["pull_request"]["checks_head_oid"], graph_head);
        assert_eq!(
            value["pull_request"]["checks"]["items"][0]["status"],
            "IN_PROGRESS"
        );
        assert_eq!(value["pull_request"]["comments"]["truncated"], true);
        assert_eq!(
            value["pull_request"]["discussion_review_decision"],
            "REVIEW_REQUIRED"
        );
        assert!(valid_pr_view_output(&result, pr_view_identity()));
    }

    #[test]
    fn host_gh_pr_view_rejects_mismatched_identity_and_graph_errors() {
        let head = "a".repeat(40);
        let graph = pr_view_graph(&head, "COMPLETED");
        let result = pr_view_result(
            &pr_view_scalar(&head),
            &graph.to_string(),
            pr_view_identity(),
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        for (field, replacement) in [
            ("number", json!(18)),
            ("url", json!("https://github.com/elsewhere/project/pull/17")),
            ("stale", json!(true)),
        ] {
            let mut invalid: Value = serde_json::from_str(&result).unwrap();
            invalid["pull_request"][field] = replacement;
            assert!(!valid_pr_view_output(
                &invalid.to_string(),
                pr_view_identity()
            ));
        }

        for url in [
            "http://github.com/example/project/pull/17",
            "https://elsewhere.com/example/project/pull/17",
            "https://github.com/example/project/issues/17",
            "https://github.com/example/project/pull/18",
        ] {
            let mut invalid = pr_view_scalar(&head);
            invalid["url"] = json!(url);
            assert!(
                pr_view_result(
                    &invalid,
                    &graph.to_string(),
                    pr_view_identity(),
                    crate::tools::types::MAX_TOOL_RESULT_BYTES,
                )
                .is_err_and(|error| error == PrViewResultError::Invalid),
                "accepted URL: {url}"
            );
        }

        let mut partial = graph;
        partial["errors"] = json!([{"message":"partial response"}]);
        assert!(
            pr_view_result(
                &pr_view_scalar(&head),
                &partial.to_string(),
                pr_view_identity(),
                crate::tools::types::MAX_TOOL_RESULT_BYTES,
            )
            .is_err_and(|error| error == PrViewResultError::Invalid)
        );
    }

    #[test]
    fn host_gh_pr_view_accepts_missing_checks_and_preserves_failures() {
        let head = "a".repeat(40);
        let mut no_checks = pr_view_graph(&head, "COMPLETED");
        no_checks["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] =
            Value::Null;
        let no_checks = pr_view_result(
            &pr_view_scalar(&head),
            &no_checks.to_string(),
            pr_view_identity(),
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        assert!(
            serde_json::from_str::<Value>(&no_checks).unwrap()["pull_request"]["checks"].is_null()
        );

        let mut failed_check = pr_view_graph(&head, "COMPLETED");
        failed_check["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["nodes"][0]["conclusion"] = json!("FAILURE");
        let failed_check = pr_view_result(
            &pr_view_scalar(&head),
            &failed_check.to_string(),
            pr_view_identity(),
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&failed_check).unwrap()["pull_request"]["checks"]["items"]
                [0]["conclusion"],
            "FAILURE"
        );
    }

    fn intent(workspace: &Path, call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: Some(pin(workspace, &call).unwrap()),
            wrap: None,
            rewrite: Vec::new(),
            host_command: None,
            target: crate::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::RunId::new(),
            agent_run_id: crate::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits {
                network: NetworkGrant::Host,
                runtime_ms: 10_000,
                ..ToolLimits::default()
            },
            use_count: 1,
        }
    }

    async fn prepare_for(
        workspace: &Path,
        intent: &EffectIntent,
    ) -> Result<(Command, String, Option<PrViewEnvironment>), String> {
        let home = workspace.parent().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let environment = super::super::tests::environment(workspace, &home.join("editors"), &home);
        prepare(
            &config(),
            &environment,
            intent,
            tokio::time::Instant::now() + Duration::from_secs(30),
        )
        .await
    }

    fn config() -> Config {
        let git = super::super::tests::git_program();
        let executable = std::fs::canonicalize("/usr/bin/true").unwrap();
        let gh = crate::tools::config::Program {
            name: "gh".into(),
            executable: executable.to_str().unwrap().into(),
            sha256: hex_digest(&std::fs::read(&executable).unwrap()),
            interpreter: false,
            inputs: Vec::new(),
        };
        serde_json::from_value(json!({"workspace_paths":["."],"write":true,"commands":[gh],"skills":[],"mcp":[],"git":git})).unwrap()
    }

    fn synthetic_json_peer(prepared: &Command, value: &Value) -> Command {
        synthetic_peer(
            prepared,
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                "printf '%s' \"$1\"".into(),
                "peer".into(),
                value.to_string().into(),
            ],
        )
    }

    fn synthetic_peer(prepared: &Command, program: &Path, args: &[std::ffi::OsString]) -> Command {
        let prepared = prepared.as_std();
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .current_dir(prepared.get_current_dir().unwrap());
        for (key, value) in prepared.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        command
    }

    #[test]
    fn host_git_closed_inputs_reject_credentials_local_transports_and_routing_syntax() {
        for remote in [
            "origin",
            "/tmp/repo",
            "file:///repo",
            "ext::sh command",
            "https://user:secret@github.com/o/r",
            "https://github.com/o/r?token=x",
            "ssh://user@host/o/r",
            "https://github.com:443/o/r",
            "https://github.com/o/../r",
            "https://github.com/o/r\n",
            "-u",
        ] {
            assert!(!valid(&ToolCall::GitPush {
                remote: remote.into(),
                branch: "main".into()
            }));
        }
        assert!(valid(&push()));
        assert!(valid(&pr()));
        for branch in ["-main", "main:other", "+main", "HEAD", "main~1", "main@{1}"] {
            assert!(!valid(&ToolCall::GitPush {
                remote: "https://host/o/r".into(),
                branch: branch.into()
            }));
        }
        let mut oversized = pr();
        if let ToolCall::GhPrCreate { title, .. } = &mut oversized {
            *title = "x".repeat(257);
        }
        assert!(!valid(&oversized));
    }

    #[test]
    fn host_only_single_exact_non_fast_forward_is_definitely_failed() {
        let f = crate::session::host_protocol_fixture();
        let intent = f
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::Event::ToolStarted { intent, .. }
                    if matches!(intent.call, ToolCall::GitPush { .. }) =>
                {
                    Some(EffectIntent::from(intent.clone()))
                }
                _ => None,
            })
            .expect("folded host push start");
        let row = format!(
            "!\t{}\t[rejected] (non-fast-forward)\n",
            intent.host.as_ref().unwrap().refspec
        );
        assert!(non_fast_forward(&intent, row.as_bytes()));
        for output in [
            "authentication refused",
            "PR already exists",
            "",
            &format!("{row}{row}"),
            &row.replace("refs/heads/main", "refs/heads/other"),
        ] {
            assert!(!non_fast_forward(&intent, output.as_bytes()));
        }
        let mut pr_intent = intent.clone();
        pr_intent.call = pr();
        assert!(!non_fast_forward(&pr_intent, row.as_bytes()));
    }

    #[tokio::test]
    async fn host_git_local_bare_peer_receipt_and_credential_canary() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let home = temp.path().join("home");
        let bare = temp.path().join("remote.git");
        host_git(
            temp.path(),
            &home,
            &["init", "--bare", "-q", bare.to_str().unwrap()],
        );
        let index = std::fs::OpenOptions::new()
            .write(true)
            .open(workspace.join(".git/index"))
            .unwrap();
        index
            .set_len((crate::tools::types::MAX_SNAPSHOT_BYTES + 1) as u64)
            .unwrap();
        let intent = intent(&workspace, push());
        assert!(intent.host.is_some());
        let markers = arm_hostile(temp.path(), &workspace);
        let large = std::fs::File::create(workspace.join(".git/objects/pack/unused.keep")).unwrap();
        large.set_len(33 * 1024 * 1024).unwrap();
        let extra = workspace.join(".git/objects/extra");
        std::fs::create_dir(&extra).unwrap();
        for n in 0..4100 {
            std::fs::write(extra.join(n.to_string()), b"x").unwrap();
        }
        let config = config();
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let args: Vec<_> = prepared.as_std().get_args().map(|s| s.to_owned()).collect();
        assert_eq!(
            args[args.len() - 2],
            "https://github.com/example/project.git"
        );
        assert_eq!(
            args.last().unwrap(),
            intent.host.as_ref().unwrap().refspec.as_str()
        );
        assert!(
            !args
                .iter()
                .any(|a| a == "--force" || a == "--force-with-lease")
        );
        let mut local_args = args;
        let last = local_args.len() - 2;
        local_args[last] = bare.as_os_str().to_owned();
        let command = synthetic_peer(
            &prepared,
            Path::new(&config.git.as_ref().unwrap().executable),
            &local_args,
        );
        let observation = supervise(command, intent.clone(), digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.valid());
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Host(_))
        ));
        let remote = host_git_output(&bare, &home, &["rev-parse", "refs/heads/main"]);
        assert_eq!(
            String::from_utf8(remote.stdout.clone()).unwrap().trim(),
            intent.host.as_ref().unwrap().source_oid
        );
        assert_eq!(
            super::super::tests::caged_markers(&markers),
            Vec::<String>::new()
        );
        assert!(
            !serde_json::to_string(&observation)
                .unwrap()
                .contains("credential-canary")
        );
        assert!(
            !crate::tools::types::model_observation(&observation)
                .to_string()
                .contains("credential-canary")
        );
        assert_eq!(pin(&workspace, &push()).unwrap(), intent.host.unwrap());
        host_git(
            &workspace,
            &home,
            &["update-ref", "refs/heads/main", "HEAD~1"],
        );
        let rejected = self::intent(&workspace, push());
        let (prepared, digest, _) = prepare_for(&workspace, &rejected).await.unwrap();
        let mut args: Vec<_> = prepared.as_std().get_args().map(|s| s.to_owned()).collect();
        let destination = args.len() - 2;
        args[destination] = bare.as_os_str().to_owned();
        let command = synthetic_peer(
            &prepared,
            Path::new(&config.git.as_ref().unwrap().executable),
            &args,
        );
        let observed = supervise(command, rejected, digest, Cancellation::new()).await;
        assert_eq!(observed.disposition, ToolDisposition::Failed);
        assert_eq!(observed.output, NON_FAST_FORWARD);
        assert!(observed.valid());
        assert_eq!(
            host_git_output(&bare, &home, &["rev-parse", "refs/heads/main"]).stdout,
            remote.stdout
        );
    }

    #[tokio::test]
    async fn host_git_fake_gh_pins_payload_and_discards_credential_output() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(&workspace, pr());
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let args: Vec<_> = prepared.as_std().get_args().map(|s| s.to_owned()).collect();
        assert_eq!(
            args,
            [
                "pr",
                "create",
                "--repo",
                "github.com/example/project",
                "--base",
                "base",
                "--head",
                "main",
                "--title",
                "A title",
                "--body",
                "First line\nSecond line",
                "--draft"
            ]
            .map(std::ffi::OsString::from)
        );
        let script = "test \"$1 $2 $3 $4 $5 $6 $7 $8\" = 'pr create --repo github.com/example/project --base base --head main' || exit 7; printf credential-canary; printf credential-canary >&2; printf done > remote-effect";
        let mut fake = vec!["-c".into(), script.into(), "gh".into()];
        fake.extend(args);
        let command = synthetic_peer(&prepared, Path::new("/bin/sh"), &fake);
        let observation = supervise(command, intent, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.valid());
        // gh runs in the Workspace with the user's own gh configuration.
        assert!(workspace.join("remote-effect").is_file());
        assert!(
            !serde_json::to_string(&observation)
                .unwrap()
                .contains("credential-canary")
        );
        assert!(
            !crate::tools::types::model_observation(&observation)
                .to_string()
                .contains("credential-canary")
        );
    }

    #[tokio::test]
    async fn host_gh_pr_list_uses_fixed_bounded_args_and_translates_json() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let call = ToolCall::GhPrList {
            repository: "github.com/example/project".into(),
            state: crate::tools::types::PrState::Merged,
            base: Some("main".into()),
            head: Some("feature".into()),
            limit: 1,
        };
        let intent = intent(&workspace, call);
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let args: Vec<_> = prepared.as_std().get_args().map(|s| s.to_owned()).collect();
        assert_eq!(
            args,
            [
                "pr",
                "list",
                "--repo",
                "github.com/example/project",
                "--state",
                "merged",
                "--limit",
                "1",
                "--json",
                "number,title,url,author,state,isDraft,baseRefName,baseRefOid,headRefName,headRefOid,reviewDecision,updatedAt",
                "--base",
                "main",
                "--head",
                "feature",
            ]
            .map(std::ffi::OsString::from)
        );
        assert!(!args.iter().any(|arg| arg == "--paginate"));
        let payload = r#"[{"number":7,"title":"Fix issue","url":"https://github.com/example/project/pull/7","author":{"login":"alice"},"state":"MERGED","isDraft":false,"baseRefName":"main","baseRefOid":"abc","headRefName":"feature","headRefOid":"def","reviewDecision":null,"updatedAt":"2026-10-22T00:00:00Z"}]"#;
        let mut fake = vec![
            "-c".into(),
            format!("printf '%s' '{}'", payload).into(),
            "gh".into(),
        ];
        fake.extend(args);
        let command = synthetic_peer(&prepared, Path::new("/bin/sh"), &fake);
        let observation = supervise(command, intent, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.valid());
        assert!(observation.output.contains("\"possibly_more\":true"));
        assert!(observation.output.contains("\"review_decision\":null"));
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Host(_))
        ));
        assert!(!observation.output.contains("credential-canary"));
    }

    #[test]
    fn host_gh_pr_list_bounds_and_default_null_fields() {
        let call: ToolCall = serde_json::from_value(json!({
            "operation":"gh_pr_list",
            "repository":"github.com/example/project",
            "state":null,
            "base":null,
            "head":null,
            "limit":null
        }))
        .unwrap();
        assert_eq!(call, pr_list());
        assert!(valid(&call));
        assert_eq!(serde_json::to_value(&call).unwrap()["state"], "open");
        assert!(
            serde_json::from_value::<ToolCall>(json!({
                "operation":"gh_pr_list",
                "repository":"github.com/example/project",
                "state":"unexpected",
                "base":null,
                "head":null,
                "limit":null
            }))
            .is_err()
        );
        for limit in [0, 51] {
            let mut invalid = pr_list();
            if let ToolCall::GhPrList { limit: value, .. } = &mut invalid {
                *value = limit;
            }
            assert!(!valid(&invalid));
        }
        assert!(
            pr_list_result(
                b"[]",
                DEFAULT_PR_LIMIT,
                crate::tools::types::MAX_TOOL_RESULT_BYTES,
            )
            .unwrap()
            .contains("\"possibly_more\":false")
        );
        assert!(pr_list_result(b"[]", 0, 1024).is_none());
        assert!(pr_list_result(b"not json", DEFAULT_PR_LIMIT, 1024).is_none());
        assert!(valid_pr_list_output(
            r#"{"pull_requests":[],"possibly_more":false,"truncated":false}"#,
            DEFAULT_PR_LIMIT
        ));
        assert!(!valid_pr_list_output("not json", DEFAULT_PR_LIMIT));
        let row = json!([{
            "number": 1,
            "title": format!("{}🧪", "a".repeat(PR_LIST_TITLE_BYTES - 1)),
            "url": "url",
            "author": {"login":"author"},
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "baseRefOid": "base",
            "headRefName": "topic",
            "headRefOid": "head",
            "reviewDecision": null,
            "updatedAt": "now"
        }]);
        let output = pr_list_result(
            &serde_json::to_vec(&row).unwrap(),
            1,
            crate::tools::types::MAX_TOOL_RESULT_BYTES,
        )
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        let title = output["pull_requests"][0]["title"].as_str().unwrap();
        assert_eq!(title.len(), PR_LIST_TITLE_BYTES - 1);
        assert_eq!(
            output["pull_requests"][0]["truncated_fields"],
            json!(["title"])
        );
        assert!(valid_pr_list_output(&output.to_string(), 1));
    }

    #[test]
    fn host_pr_list_preparation_refusal_fits_the_minimum_result_budget() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let mut intent = intent(&workspace, pr_list());
        intent.limits.result_bytes = 128;
        let refusal = observation(intent, ToolDisposition::Denied, PREPARATION_LIMIT, None);
        assert_eq!(refusal.disposition, ToolDisposition::Denied);
        assert!(refusal.output.contains("nothing dispatched"));
        assert!(refusal.valid());
    }

    #[test]
    fn host_gh_pr_list_truncates_whole_rows_to_the_pinned_result_budget() {
        let rows = (1..=3)
            .map(|number| {
                json!({
                    "number":number,
                    "title":"a".repeat(200),
                    "url":"https://github.com/example/project/pull/1",
                    "author":{"login":"octocat"},
                    "state":"OPEN",
                    "isDraft":false,
                    "baseRefName":"main",
                    "baseRefOid":"base-sha",
                    "headRefName":"topic",
                    "headRefOid":"head-sha",
                    "reviewDecision":"APPROVED",
                    "updatedAt":"2026-10-25T12:00:00Z"
                })
            })
            .collect::<Vec<_>>();
        let budget = 700;
        let output = pr_list_result(&serde_json::to_vec(&rows).unwrap(), 3, budget).unwrap();
        assert!(output.len() <= budget);
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["pull_requests"].as_array().unwrap().len(), 1);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["possibly_more"], true);
        assert!(valid_pr_list_output(&output, 3));
    }

    #[tokio::test]
    async fn host_gh_pr_list_invalid_data_is_a_confirmed_failure() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(&workspace, pr_list());
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let command = synthetic_peer(
            &prepared,
            Path::new("/bin/sh"),
            &["-c".into(), "printf 'not json'".into()],
        );
        let observation = supervise(command, intent, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert_eq!(observation.output, PR_LIST_INVALID);
        assert!(observation.valid());
    }

    #[tokio::test]
    async fn host_gh_pr_view_nonzero_exit_uses_inspection_failure_text() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(
            &workspace,
            ToolCall::GhPrView {
                repository: "github.com/example/project".into(),
                number: 17,
            },
        );
        let (prepared, digest, environment) = prepare_for(&workspace, &intent).await.unwrap();
        let input = environment.unwrap().input;
        let (writer, reader) = tokio::net::UnixStream::pair().unwrap();
        drop(reader);
        write_process_input(writer, &input).await.unwrap();
        let mut command = synthetic_peer(
            &prepared,
            Path::new("/bin/sh"),
            &["-c".into(), "exit 7".into()],
        );
        command.stdin(Stdio::piped());
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.effective_timeout_ms()));
        let observation = supervise_input(ProcessDispatch {
            deadline,
            command,
            intent,
            executable_sha256: digest,
            cancellation: Cancellation::new(),
            input: Some(input),
        })
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert_eq!(observation.output, PR_VIEW_FAILED);
        assert!(observation.valid());
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Host(ref receipt))
                if receipt.exit_code == Some(7) && receipt.descendants_confirmed_stopped
        ));
        let mut wrong_operation = observation;
        wrong_operation.output = PR_LIST_FAILED.into();
        assert!(!wrong_operation.valid());
    }

    #[tokio::test]
    async fn host_gh_pr_view_small_result_budget_is_a_confirmed_local_failure() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let mut intent = intent(
            &workspace,
            ToolCall::GhPrView {
                repository: "github.com/example/project".into(),
                number: 17,
            },
        );
        intent.limits.result_bytes = 128;
        let (prepared, digest, environment) = prepare_for(&workspace, &intent).await.unwrap();
        let mut environment = environment.unwrap();
        let head = "a".repeat(40);
        let scalar_json = pr_view_scalar(&head);
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.effective_timeout_ms()));
        let scalar = super::supervise(
            deadline,
            synthetic_json_peer(&prepared, &scalar_json),
            intent.clone(),
            digest.clone(),
            Cancellation::new(),
        )
        .await;
        assert_eq!(scalar.disposition, ToolDisposition::Succeeded);
        environment.command =
            synthetic_json_peer(&environment.command, &pr_view_graph(&head, "COMPLETED"));
        environment.command.stdin(Stdio::piped());
        let observation = supervise_pr_view_details(PrViewDetails {
            deadline,
            environment,
            executable: digest,
            intent,
            scalar,
            scalar_json,
            identity: pr_view_identity(),
            cancellation: Cancellation::new(),
        })
        .await;
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert_eq!(observation.output, PR_VIEW_RESULT_BUDGET);
        assert!(observation.valid());
        assert!(observation.output.len() <= observation.intent.limits.result_bytes as usize);
        assert!(matches!(
            observation.attestation,
            Some(Attestation::Host(ref receipt))
                if receipt.exit_code == Some(0) && receipt.descendants_confirmed_stopped
        ));
    }

    #[tokio::test]
    async fn host_gh_pr_list_capture_overflow_is_a_confirmed_failure() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(&workspace, pr_list());
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let command = synthetic_peer(
            &prepared,
            Path::new("/bin/sh"),
            &["-c".into(), "printf '%262145s' ''".into()],
        );
        let observation = supervise(command, intent, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert_eq!(observation.output, PR_LIST_CAPTURE_LIMIT);
        assert!(observation.valid());
        let Some(Attestation::Host(receipt)) = observation.attestation else {
            panic!("bounded host receipt");
        };
        assert!(receipt.descendants_confirmed_stopped);
    }

    #[tokio::test]
    async fn host_gh_pr_list_timeout_stays_uncertain_after_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(&workspace, pr_list());
        let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
        let (ready, child_ready) = std::os::unix::net::UnixStream::pair().unwrap();
        ready.set_nonblocking(true).unwrap();
        let mut ready = tokio::net::UnixStream::from_std(ready).unwrap();
        let mut command = synthetic_peer(
            &prepared,
            Path::new("/bin/sh"),
            &["-c".into(), "printf ready >&0; read -r release".into()],
        );
        command.stdin(Stdio::from(std::os::fd::OwnedFd::from(child_ready)));
        let timeout = Duration::from_millis(u64::from(intent.effective_timeout_ms()));
        let deadline = tokio::time::Instant::now() + timeout;
        let gate = async {
            let mut signal = [0; 5];
            tokio::time::timeout(Duration::from_secs(30), ready.read_exact(&mut signal))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&signal, b"ready");
            tokio::time::pause();
            tokio::time::advance(timeout).await;
            tokio::time::resume();
        };
        let (observation, ()) = tokio::join!(
            super::supervise(deadline, command, intent, digest, Cancellation::new()),
            gate
        );
        assert_eq!(observation.disposition, ToolDisposition::Uncertain);
        assert_eq!(observation.output, UNKNOWN_HOST_GIT_OUTCOME);
        assert!(observation.valid());
        let Some(Attestation::Host(receipt)) = observation.attestation else {
            panic!("bounded host receipt");
        };
        assert!(receipt.descendants_confirmed_stopped);
        assert_eq!(receipt.exit_code, None);
    }

    #[tokio::test]
    async fn host_git_lost_ack_and_output_overflow_are_uncertain_without_retry() {
        for script in [
            "printf once >> remote-effect; exit 7",
            "printf once >> remote-effect; printf '%262145s' ''",
            "printf once >> remote-effect; printf '%4097s' '' >&2",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = fixture(temp.path());
            let intent = intent(&workspace, pr());
            let (prepared, digest, _) = prepare_for(&workspace, &intent).await.unwrap();
            let command = synthetic_peer(
                &prepared,
                Path::new("/bin/sh"),
                &["-c".into(), script.into()],
            );
            let observation = supervise(command, intent, digest, Cancellation::new()).await;
            assert_eq!(observation.disposition, ToolDisposition::Uncertain);
            assert!(observation.valid() && observation.stops_run());
            assert_eq!(
                std::fs::read(workspace.join("remote-effect")).unwrap(),
                b"once"
            );
        }
    }

    #[tokio::test]
    async fn host_git_supervision_cancellation_and_preparation_runtime_timeouts() {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("effect");
            let intent = EffectIntent {
                host: Some(HostIntent {
                    source_oid: "1".repeat(40),
                    refspec: format!("{}:refs/heads/main", "1".repeat(40)),
                }),
                wrap: None,
                rewrite: Vec::new(),
                host_command: None,
                target: crate::worker::EffectTarget::Workspace,
                id: uuid::Uuid::now_v7(),
                run_id: crate::RunId::new(),
                agent_run_id: crate::AgentRunId::new(),
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call: push(),
                limits: ToolLimits {
                    runtime_ms: 600_000,
                    network: NetworkGrant::Host,
                    ..ToolLimits::default()
                },
                use_count: 1,
            };
            let (ready, child_ready) = std::os::unix::net::UnixStream::pair().unwrap();
            ready.set_nonblocking(true).unwrap();
            let mut ready = tokio::net::UnixStream::from_std(ready).unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "exec 3<&0; (printf once > effect; printf credential-canary; printf ready >&3; read -r release <&3) & exec 0<&- 3<&-; wait",
                ])
                .current_dir(root.path())
                .env_clear()
                .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_ready)))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .kill_on_drop(true);
            let cancellation = Cancellation::new();
            let trigger = cancellation.clone();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            let gate = async {
                let mut signal = [0; 5];
                tokio::time::timeout(Duration::from_secs(30), ready.read_exact(&mut signal))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&signal, b"ready");
                if cancel {
                    trigger.cancel();
                } else {
                    // Advance only after the real child has completed its effect; reap on real time.
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(300)).await;
                    tokio::time::resume();
                }
            };
            let (observation, ()) = tokio::join!(
                super::supervise(
                    deadline,
                    command,
                    intent.clone(),
                    "0".repeat(64),
                    cancellation
                ),
                gate
            );
            let mut eof = [0; 1];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(30), ready.read(&mut eof))
                    .await
                    .expect("controlled descendant must close its socket")
                    .unwrap(),
                0
            );
            assert_eq!(observation.disposition, ToolDisposition::Uncertain);
            assert!(observation.valid() && observation.stops_run());
            assert!(matches!(
                &observation.attestation,
                Some(Attestation::Host(receipt)) if receipt.descendants_confirmed_stopped
            ));
            assert_eq!(std::fs::read(&marker).unwrap(), b"once");
            assert!(
                !serde_json::to_string(&observation)
                    .unwrap()
                    .contains("credential-canary")
            );
            assert!(
                !crate::tools::types::model_observation(&observation)
                    .to_string()
                    .contains("credential-canary")
            );
            for cancelled in [false, true] {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "printf twice >> effect"])
                    .current_dir(root.path());
                let cancellation = Cancellation::new();
                if cancelled {
                    cancellation.cancel();
                }
                let denied = super::supervise(
                    tokio::time::Instant::now(),
                    command,
                    intent.clone(),
                    "0".repeat(64),
                    cancellation,
                )
                .await;
                assert_eq!(denied.disposition, ToolDisposition::Denied);
                assert!(denied.attestation.is_none());
                assert_eq!(
                    denied.output,
                    if cancelled {
                        super::CANCELLED
                    } else {
                        super::PREPARATION_TIMEOUT
                    }
                );
                assert_eq!(std::fs::read(&marker).unwrap(), b"once");
            }
        }
    }
}

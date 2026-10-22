use super::{
    CAPTURE_BYTES, STDERR_BYTES, VIEW_CONFIG, inspect_layout, read_optional, valid_branch,
};
use crate::tools::config::Config;
use crate::tools::types::{EffectIntent, HostIntent, ToolCall, ToolDisposition, ToolObservation};
use crate::tools::{Attestation, CacheUpdate, Cancellation, ToolError, UnguardedReceipt};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const UNKNOWN: &str =
    "host: remote outcome unknown. Inspect remote state; never retry automatically.";
pub(crate) const NON_FAST_FORWARD: &str = "host: remote reported no ref update (non-fast-forward).";
const REFUSED: &str = "host: destination, source, authentication, pinned executable or repository layout unavailable (HEAD limit 256 B; repository config 64 KiB); nothing dispatched.";
const PREPARATION_TIMEOUT: &str =
    "host: preparation exceeded the runtime deadline; nothing dispatched.";
const CANCELLED: &str = "host: approval expired or cancelled; nothing dispatched.";
const MAX_PRIVATE_BYTES: usize = 32 * 1024 * 1024;

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

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    let string = || json!({"type":"string"});
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
        _ => return None,
    })
}

pub(crate) fn valid_pin(call: &ToolCall, pin: &HostIntent) -> bool {
    let branch = match call {
        ToolCall::GitPush { branch, .. } => branch,
        ToolCall::GhPrCreate { head, .. } => head,
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
    let repository = inspect_layout(workspace)?.map_err(|_| ToolError::Path)?;
    pin_from_git(&repository.git_dir, call)
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
                read_optional(git, "packed-refs", MAX_PRIVATE_BYTES)?.ok_or(ToolError::Path)?;
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

fn objects_identity(git: &Dir) -> Result<(u64, u64), ToolError> {
    let objects = git
        .open_dir_nofollow("objects")
        .map_err(|_| ToolError::Path)?;
    if let Ok(info) = objects.open_dir_nofollow("info") {
        for name in ["alternates", "http-alternates"] {
            if info.symlink_metadata(name).is_ok() {
                return Err(ToolError::Path);
            }
        }
    } else if objects.symlink_metadata("info").is_ok() {
        return Err(ToolError::Path);
    }
    crate::tools::fs::identity(&objects)
}

fn backing_identity(workspace: &Path) -> Result<[(u64, u64); 2], ToolError> {
    let root = crate::tools::fs::open_directory(workspace)?;
    let git = root
        .open_dir_nofollow(".git")
        .map_err(|_| ToolError::Path)?;
    Ok([crate::tools::fs::identity(&git)?, objects_identity(&git)?])
}

fn private_view(
    workspace: &Path,
    root: &Path,
    intent: &EffectIntent,
) -> Result<PathBuf, ToolError> {
    let repository = inspect_layout(workspace)?.map_err(|_| ToolError::Path)?;
    if Some(&pin_from_git(&repository.git_dir, &intent.call)?) != intent.host.as_ref() {
        return Err(ToolError::ChangedInput);
    }
    let view = root.join(".git");
    std::fs::create_dir(&view).map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("config"), VIEW_CONFIG).map_err(|_| ToolError::Operation)?;
    if matches!(intent.call, ToolCall::GitPush { .. }) {
        objects_identity(&repository.git_dir)?;
        std::os::unix::fs::symlink(workspace.join(".git/objects"), view.join("objects"))
            .map_err(|_| ToolError::Operation)?;
        if let Some(bytes) = read_optional(&repository.git_dir, "shallow", MAX_PRIVATE_BYTES)? {
            std::fs::write(view.join("shallow"), bytes).map_err(|_| ToolError::Operation)?;
        }
    } else {
        std::fs::create_dir(view.join("objects")).map_err(|_| ToolError::Operation)?;
    }
    let branch = match &intent.call {
        ToolCall::GitPush { branch, .. } => branch,
        ToolCall::GhPrCreate { head, .. } => head,
        _ => return Err(ToolError::Configuration),
    };
    let pinned = intent.host.as_ref().ok_or(ToolError::Configuration)?;
    let reference = view.join("refs/heads").join(branch);
    std::fs::create_dir_all(reference.parent().ok_or(ToolError::Path)?)
        .map_err(|_| ToolError::Operation)?;
    std::fs::write(reference, format!("{}\n", pinned.source_oid))
        .map_err(|_| ToolError::Operation)?;
    std::fs::write(view.join("HEAD"), format!("ref: refs/heads/{branch}\n"))
        .map_err(|_| ToolError::Operation)?;
    Ok(view)
}

#[derive(Clone)]
pub(crate) struct Authentication {
    home: PathBuf,
    git_config: Vec<Vec<u8>>,
    ssh_agent: Option<std::ffi::OsString>,
    gh_hosts: Option<Vec<u8>>,
    gh_host: String,
}

impl Authentication {
    pub(crate) fn admit(config: &Config) -> Option<Self> {
        if !config.write || !config.git_available() {
            return None;
        }
        let home = directories::BaseDirs::new()?.home_dir().to_owned();
        let gh_dir = std::env::var_os("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| home.join(".config"))
                    .join("gh")
            });
        let gh_hosts = auth_file(&gh_dir.join("hosts.yml"));
        let settings = auth_file(&gh_dir.join("config.yml"));
        let gh_host = settings
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| text.lines().find_map(|line| line.strip_prefix("host: ")))
            .unwrap_or("github.com")
            .to_owned();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        Some(Self {
            git_config: [xdg.join("git/config"), home.join(".gitconfig")]
                .iter()
                .filter_map(|p| auth_file(p))
                .collect(),
            home,
            ssh_agent: std::env::var_os("SSH_AUTH_SOCK"),
            gh_hosts,
            gh_host,
        })
    }

    fn install_gh(&self, root: &Path) -> Result<(), ToolError> {
        if let Some(bytes) = &self.gh_hosts {
            let directory = root.join("gh");
            private_directory(&directory)?;
            private_file(&directory.join("hosts.yml"), bytes, 0o600)?;
        }
        Ok(())
    }

    pub(crate) fn offers(&self, operation: &str) -> bool {
        match operation {
            "git_push" => !self.git_config.is_empty() || self.ssh_agent.is_some(),
            "gh_pr_create" => self.gh_hosts.is_some(),
            _ => false,
        }
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        match call {
            ToolCall::GitPush { remote, .. } => destination(remote).is_some_and(|(_, ssh)| {
                if ssh {
                    self.ssh_agent.is_some()
                } else {
                    !self.git_config.is_empty()
                }
            }),
            ToolCall::GhPrCreate { repository, .. } => {
                self.gh_hosts.is_some()
                    && repository_parts(repository).is_some_and(|(host, _, _)| host == self.gh_host)
            }
            _ => false,
        }
    }
}

fn auth_file(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let file = crate::tools::fs::open_absolute(path.to_str()?).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return None;
    }
    crate::tools::fs::read_regular(file, 64 * 1024).ok()
}

fn private_directory(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| ToolError::Operation)
}

fn private_file(path: &Path, bytes: &[u8], mode: u32) -> Result<std::fs::File, ToolError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|_| ToolError::Operation)?;
    file.write_all(bytes).map_err(|_| ToolError::Operation)?;
    Ok(file)
}

const SCRATCH_SLOTS: usize = 64;

fn scratch_parent(
    state: &crate::store::StateRoot,
    create: bool,
) -> Result<crate::private_fs::PrivateDirectory, ToolError> {
    state.readmit().map_err(|_| ToolError::Path)?;
    crate::private_fs::open_directory(&state.path().join("host-scratch"), create)
        .map_err(|_| ToolError::Path)
}

fn slot_lease(parent: &Dir, slot: usize) -> Result<Option<std::fs::File>, ToolError> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::OpenOptionsExt;
    use std::os::unix::fs::MetadataExt;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .follow(FollowSymlinks::No)
        .custom_flags(nix::libc::O_NONBLOCK);
    let file = parent
        .open_with(format!("lease-{slot}"), &options)
        .map_err(|_| ToolError::Path)?
        .into_std();
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(ToolError::Path);
    }
    crate::private_fs::check_acl(&file, true).map_err(|_| ToolError::Path)?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(_) => Err(ToolError::Operation),
    }
}

fn remove_slot(parent: &Dir, slot: usize) -> Result<(), ToolError> {
    let name = format!("slot-{slot}");
    match parent.open_dir_nofollow(&name) {
        Ok(dir) => crate::private_fs::check_directory(&dir, true).map_err(|_| ToolError::Path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ToolError::Path),
    }
    parent
        .remove_dir_all(name)
        .map_err(|_| ToolError::Operation)
}

struct Scratch {
    parent: crate::private_fs::PrivateDirectory,
    slot: usize,
    path: PathBuf,
    _lease: std::fs::File,
}

impl Scratch {
    fn acquire(state: &crate::store::StateRoot) -> Result<Self, ToolError> {
        let parent = scratch_parent(state, true)?;
        for slot in 0..SCRATCH_SLOTS {
            let Some(lease) = slot_lease(&parent.dir, slot)? else {
                continue;
            };
            remove_slot(&parent.dir, slot)?;
            let path = parent.path.join(format!("slot-{slot}"));
            // The lease is outside the root and locked before publishing any scratch directory.
            private_directory(&path)?;
            return Ok(Self {
                parent,
                slot,
                path,
                _lease: lease,
            });
        }
        Err(ToolError::Limit)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = remove_slot(&self.parent.dir, self.slot);
    }
}

pub(crate) fn sweep_stale(state: &crate::store::StateRoot) {
    let Ok(parent) = scratch_parent(state, false) else {
        return;
    };
    for slot in 0..SCRATCH_SLOTS {
        if let Ok(Some(_lease)) = slot_lease(&parent.dir, slot) {
            let _ = remove_slot(&parent.dir, slot);
        }
    }
}

fn verified_copy(program: &crate::tools::config::Program, target: &Path) -> Result<(), ToolError> {
    let mut file = private_file(target, &[], 0o500)?;
    let result = program.verify_executable_to(&mut file);
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

fn helper_path(directories: &[&str]) -> Result<String, ToolError> {
    use cap_fs_ext::OsMetadataExt;
    let mut paths = Vec::new();
    for path in directories {
        let Ok(path) = std::fs::canonicalize(path) else {
            continue;
        };
        let Ok(directory) = crate::tools::fs::open_directory(&path) else {
            continue;
        };
        let Ok(metadata) = directory.dir_metadata() else {
            continue;
        };
        // The explicit search never inherits a repository-controlled PATH component.
        if metadata.mode() & 0o022 != 0
            || ![0, rustix::process::geteuid().as_raw()].contains(&metadata.uid())
        {
            continue;
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(ToolError::HostHelperSearch);
    }
    std::env::join_paths(paths)
        .map_err(|_| ToolError::Path)?
        .into_string()
        .map_err(|_| ToolError::Path)
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

async fn credential_config(
    git: &crate::tools::config::Program,
    files: &[Vec<u8>],
    root: &Path,
) -> Result<Vec<(String, String)>, ToolError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut entries = Vec::new();
    for (index, file) in files.iter().enumerate() {
        entries.extend(read_credentials(git, file, root, index, deadline).await?);
        if entries.len() > 256
            || entries
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>()
                > 128 * 1024
        {
            return Err(ToolError::Limit);
        }
    }
    if !entries.iter().any(|(key, value)| {
        (key == "credential.helper" || key.ends_with(".helper")) && !value.is_empty()
    }) {
        return Err(ToolError::Configuration);
    }
    Ok(entries)
}

async fn read_credentials(
    git: &crate::tools::config::Program,
    bytes: &[u8],
    root: &Path,
    index: usize,
    deadline: tokio::time::Instant,
) -> Result<Vec<(String, String)>, ToolError> {
    let config = root.join(format!("credential-config-{index}"));
    private_file(&config, bytes, 0o600)?;
    let mut child = Command::new(&git.executable)
        .args(["config", "--file"])
        .arg(&config)
        .args([
            "--no-includes",
            "--null",
            "--get-regexp",
            r"^(credential\.|include\.|includeif\.)",
        ])
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_DIR", root.join(".git"))
        .env("GIT_CEILING_DIRECTORIES", root)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ToolError::Configuration)?;
    let out = child.stdout.take().ok_or(ToolError::Operation)?;
    let err = child.stderr.take().ok_or(ToolError::Operation)?;
    let result = tokio::time::timeout_at(deadline, async {
        tokio::try_join!(
            async { child.wait().await.map_err(|_| ToolError::Operation) },
            capture(out, 128 * 1024),
            discard(err, STDERR_BYTES)
        )
    })
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    let (status, bytes, ()) = result.map_err(|_| ToolError::Limit)??;
    if status.code() == Some(1) && bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !status.success() {
        return Err(ToolError::Configuration);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ToolError::Configuration)?;
    let text = text.strip_suffix('\0').ok_or(ToolError::Configuration)?;
    let mut entries = Vec::new();
    for entry in text.split('\0') {
        let (key, value) = match entry.split_once('\n') {
            Some(pair) => pair,
            None if entry.ends_with(".usehttppath") || entry.ends_with(".interactive") => {
                (entry, "true")
            }
            None if entry.starts_with("include.") || entry.starts_with("includeif.") => {
                return Err(ToolError::CredentialIncludes);
            }
            None => return Err(ToolError::Configuration),
        };
        if !key.starts_with("credential.") {
            return Err(ToolError::CredentialIncludes);
        }
        if entries.len() == 256 {
            return Err(ToolError::Limit);
        }
        entries.push((key.to_owned(), value.to_owned()));
    }
    Ok(entries)
}

fn apply_credentials(command: &mut Command, mut entries: Vec<(String, String)>) {
    entries.push(("credential.interactive".into(), "false".into()));
    command.env("GIT_CONFIG_COUNT", entries.len().to_string());
    for (index, (key, value)) in entries.into_iter().enumerate() {
        command.env(format!("GIT_CONFIG_KEY_{index}"), key);
        command.env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
}

fn preparation_refusal(error: &ToolError) -> &'static str {
    match error {
        ToolError::CredentialIncludes => {
            "host: credential configuration contains include/includeIf; included credential resets cannot be safely omitted; nothing dispatched."
        }
        ToolError::HostHelperSearch => {
            "host: no admitted credential-helper search directory (requires root/user ownership and no group/other write permission); nothing dispatched."
        }
        ToolError::Limit => {
            "host: preparation limit (refs 128 B; HEAD 256 B; config 64 KiB; packed refs or shallow 32 MiB; executable 256 MiB; credential query 128 KiB/256 entries/5 seconds); nothing dispatched."
        }
        ToolError::Configuration => {
            "host: unsupported destination or authentication; configure a resolvable Git credential helper or ssh-agent and a pinned gh with existing auth; nothing dispatched."
        }
        _ => REFUSED,
    }
}

async fn prepare<R: AsRef<Path> + Send + Sync + 'static>(
    config: &Config,
    authentication: &Authentication,
    workspace: &Path,
    intent: &EffectIntent,
    root: Arc<R>,
) -> Result<(Command, String), ToolError> {
    let (mut command, digest) = {
        let config = config.clone();
        let authentication = authentication.clone();
        let workspace = workspace.to_owned();
        let intent = intent.clone();
        // Keep the scratch lease alive even if the awaiting future is dropped.
        let root = Arc::clone(&root);
        tokio::task::spawn_blocking(move || {
            prepare_command(
                &config,
                &authentication,
                &workspace,
                &intent,
                (*root).as_ref(),
            )
        })
        .await
        .map_err(|_| ToolError::Operation)??
    };
    if let ToolCall::GitPush { remote, .. } = &intent.call
        && destination(remote).is_some_and(|(_, ssh)| !ssh)
    {
        let git = config.git.as_ref().ok_or(ToolError::Configuration)?;
        apply_credentials(
            &mut command,
            credential_config(git, &authentication.git_config, (*root).as_ref()).await?,
        );
    }
    Ok((command, digest))
}

fn prepare_command(
    config: &Config,
    authentication: &Authentication,
    workspace: &Path,
    intent: &EffectIntent,
    root: &Path,
) -> Result<(Command, String), ToolError> {
    let git = config.git.as_ref().ok_or(ToolError::Configuration)?;
    super::verify_host_executable(git)?;
    if !authentication.allows(&intent.call) {
        return Err(ToolError::Configuration);
    }
    let path = helper_path(&["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"])?;
    let view = private_view(workspace, root, intent)?;
    let bin = root.join("bin");
    private_directory(&bin)?;
    std::os::unix::fs::symlink(&git.executable, bin.join("git"))
        .map_err(|_| ToolError::Operation)?;
    authentication.install_gh(root)?;
    let (program, args, auth) = match &intent.call {
        ToolCall::GitPush { remote, .. } => {
            let (_, ssh) = destination(remote).ok_or(ToolError::Configuration)?;
            let mut args = vec![
                "--no-pager".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "-c".into(),
                "http.followRedirects=false".into(),
                "-c".into(),
                "push.gpgSign=false".into(),
            ];
            let auth = if ssh {
                let socket = authentication
                    .ssh_agent
                    .clone()
                    .ok_or(ToolError::Configuration)?;
                use std::os::unix::fs::{FileTypeExt, MetadataExt};
                if !Path::new(&socket).is_absolute()
                    || !std::fs::symlink_metadata(&socket).is_ok_and(|m| {
                        m.file_type().is_socket() && m.uid() == rustix::process::geteuid().as_raw()
                    })
                {
                    return Err(ToolError::Configuration);
                }
                args.extend(["-c".into(), "core.sshCommand=/usr/bin/ssh -F /dev/null -o BatchMode=yes -o IdentityFile=none -o IdentitiesOnly=no -o StrictHostKeyChecking=yes -o ClearAllForwardings=yes".into()]);
                Some(socket)
            } else {
                None
            };
            args.extend([
                "push".into(),
                "--porcelain".into(),
                "--no-verify".into(),
                "--no-follow-tags".into(),
                "--recurse-submodules=no".into(),
                "--".into(),
                remote.clone(),
                intent
                    .host
                    .as_ref()
                    .ok_or(ToolError::Configuration)?
                    .refspec
                    .clone(),
            ]);
            (git, args, auth)
        }
        ToolCall::GhPrCreate {
            repository,
            base,
            head,
            title,
            body,
            draft,
        } => {
            let gh = config
                .commands
                .iter()
                .find(|p| p.name == "gh" && !p.interpreter && p.inputs.is_empty())
                .ok_or(ToolError::Configuration)?;
            verified_copy(gh, &root.join("gh-executable"))?;
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
            (gh, args, None)
        }
        _ => return Err(ToolError::Configuration),
    };
    let executable = if matches!(intent.call, ToolCall::GhPrCreate { .. }) {
        root.join("gh-executable")
    } else {
        PathBuf::from(&program.executable)
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", format!("{}:{path}", bin.display()))
        .env("HOME", &authentication.home)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("GIT_DIR", view)
        .env("GIT_WORK_TREE", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("SSH_ASKPASS", "/usr/bin/false")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EDITOR", "/usr/bin/false")
        .env("GH_EDITOR", "/usr/bin/false")
        .env("GH_BROWSER", "/usr/bin/false")
        .env("GH_PAGER", "cat")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_CONFIG_DIR", root.join("gh"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(socket) = auth {
        command.env("SSH_AUTH_SOCK", socket);
    }
    Ok((command, program.sha256.clone()))
}

pub(crate) async fn run(
    config: &Config,
    authentication: Option<&Authentication>,
    state: Option<&crate::store::StateRoot>,
    workspace: &Path,
    intent: EffectIntent,
    mut cancellation: Cancellation,
) -> ToolObservation {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.effective_timeout_ms()));
    let Some(authentication) = authentication else {
        return observation(intent, ToolDisposition::Denied, REFUSED, None);
    };
    let Some(state) = state else {
        return observation(intent, ToolDisposition::Denied, REFUSED, None);
    };
    let preparation = async {
        let state = state.try_clone().map_err(|_| REFUSED)?;
        let path = workspace.to_owned();
        let push = matches!(intent.call, ToolCall::GitPush { .. });
        let (root, backing) = tokio::task::spawn_blocking(move || {
            let root = Scratch::acquire(&state).map_err(|error| {
                if matches!(error, ToolError::Limit) {
                    "host: all 64 State scratch slots are in use; nothing dispatched."
                } else {
                    REFUSED
                }
            })?;
            let backing = if push {
                Some(backing_identity(&path).map_err(|_| REFUSED)?)
            } else {
                None
            };
            Ok::<_, &'static str>((Arc::new(root), backing))
        })
        .await
        .map_err(|_| REFUSED)??;
        let (command, executable) = prepare(
            config,
            authentication,
            workspace,
            &intent,
            Arc::clone(&root),
        )
        .await
        .map_err(|error| preparation_refusal(&error))?;
        Ok::<_, &'static str>((root, backing, command, executable))
    };
    let prepared = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(CANCELLED),
        result = tokio::time::timeout_at(deadline, preparation) =>
            result.unwrap_or(Err(PREPARATION_TIMEOUT)),
    };
    let (_root, backing, command, executable) = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => return observation(intent, ToolDisposition::Denied, reason, None),
    };
    let mut outcome = supervise(deadline, command, intent, executable, cancellation).await;
    if backing.is_some()
        && outcome.attestation.is_some()
        && backing_identity(workspace).ok() != backing
    {
        outcome.disposition = ToolDisposition::Uncertain;
        outcome.output = UNKNOWN.into();
    }
    outcome
}

fn observation(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: &str,
    receipt: Option<UnguardedReceipt>,
) -> ToolObservation {
    ToolObservation {
        intent,
        disposition,
        output: output.into(),
        attestation: receipt.map(Attestation::Host),
        cache_update: CacheUpdate::NotApplicable,
        enforcement_cause: None,
        limit_cause: None,
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
    mut command: Command,
    intent: EffectIntent,
    executable_sha256: String,
    mut cancellation: Cancellation,
) -> ToolObservation {
    if cancellation.is_cancelled() || crate::tools::now_ms() >= intent.expires_at_ms {
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
        tokio::select! {
            biased;
            () = cancellation.cancelled() => None,
            result = tokio::time::timeout_at(deadline, async {
                tokio::try_join!(
                    async { crate::tools::guard::reap_owned_child(&mut child).await.map_err(|_| ToolError::Operation) },
                    capture(out, CAPTURE_BYTES), discard(err, STDERR_BYTES)
                )
            }) => result.ok().and_then(Result::ok).map(|(status, bytes, ())| (status, bytes)),
        }
    } else {
        None
    };
    let status = completed.as_ref().map(|(status, _)| *status);
    let reaped = if status.is_some() {
        true
    } else {
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            crate::tools::guard::reap_owned_child(&mut child),
        )
        .await
        .is_ok_and(|r| r.is_ok())
    };
    let stopped = match group {
        Some(group) => crate::tools::unguarded::stop_group(group).await,
        None => false,
    };
    let succeeded = reaped && stopped && status.is_some_and(|s| s.success());
    let rejected = reaped
        && stopped
        && status.is_some_and(|s| s.code() == Some(1))
        && completed
            .as_ref()
            .is_some_and(|(_, bytes)| non_fast_forward(&intent, bytes));
    let receipt = UnguardedReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        executable_sha256,
        pid,
        exit_code: status.and_then(|s| s.code()),
        descendants_confirmed_stopped: reaped && stopped,
    };
    observation(
        intent,
        if succeeded {
            ToolDisposition::Succeeded
        } else if rejected {
            ToolDisposition::Failed
        } else {
            ToolDisposition::Uncertain
        },
        if succeeded {
            "host: remote operation completed."
        } else if rejected {
            NON_FAST_FORWARD
        } else {
            UNKNOWN
        },
        Some(receipt),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::git::tests::{arm_hostile, fixture, host_git, host_git_output};
    use crate::tools::types::{NetworkGrant, ToolLimits, hex_digest};

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

    fn intent(workspace: &Path, call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: Some(pin(workspace, &call).unwrap()),
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
            expires_at_ms: crate::tools::now_ms() + 60_000,
            use_count: 1,
        }
    }

    fn config() -> Config {
        let git = super::super::discover().unwrap();
        let executable = std::fs::canonicalize("/usr/bin/true").unwrap();
        let gh = crate::tools::config::Program {
            name: "gh".into(),
            executable: executable.to_str().unwrap().into(),
            sha256: hex_digest(&std::fs::read(&executable).unwrap()),
            interpreter: false,
            inputs: Vec::new(),
        };
        serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":true,"commands":[gh],"skills":[],"mcp":[],"git":git})).unwrap()
    }

    fn authentication(root: &Path) -> Authentication {
        Authentication {
            home: root.join("home"),
            git_config: vec![b"[credential]\n helper = store\n".to_vec()],
            ssh_agent: None,
            gh_hosts: Some(b"github.com:\n    oauth_token: credential-canary\n".to_vec()),
            gh_host: "github.com".into(),
        }
    }

    fn parser_git() -> crate::tools::config::Program {
        let executable = std::fs::canonicalize("/usr/bin/git").unwrap();
        crate::tools::config::Program {
            name: "git".into(),
            executable: executable.to_str().unwrap().into(),
            sha256: hex_digest(&std::fs::read(executable).unwrap()),
            interpreter: false,
            inputs: vec![],
        }
    }

    #[test]
    fn host_catalog_uses_fixture_authentication_snapshotted_at_admission() {
        use crate::test_support::process::{BoundedOutput, isolate_host_authentication};
        const CHILD: &str = "ARANY_HOST_CATALOG_FIXTURE";
        let Some(home) = std::env::var_os(CHILD) else {
            let temp = tempfile::tempdir().unwrap();
            let home = std::fs::canonicalize(temp.path()).unwrap();
            let test = std::thread::current().name().unwrap().to_owned();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .env(CHILD, &home)
                .args(["--exact", &test, "--nocapture"]);
            isolate_host_authentication(&mut command, &home);
            let output = command
                .bounded_output_for(Duration::from_secs(30), 64 * 1024)
                .unwrap();
            assert!(
                output.status.success(),
                "isolated host catalog fixture failed"
            );
            crate::test_support::process::assert_single_test_completed(&output.stdout, &test);
            return;
        };
        let home = PathBuf::from(home);
        assert!(std::env::var_os("SSH_AUTH_SOCK").is_none());
        let mut runtime = crate::tools::tests::in_process_runtime(
            &home,
            json!({
                "version": 1, "workspace_paths": ["."], "write": true,
                "commands": [], "skills": [], "mcp": [],
                "git": {"name":"git", "executable":"/usr/bin/git", "sha256":"0".repeat(64), "interpreter":false, "inputs":[]}
            }),
        );
        let operations = |runtime: &crate::tools::ToolRuntime| {
            let catalog: serde_json::Value =
                serde_json::from_str(runtime.context().unwrap().catalog()).unwrap();
            assert!(!catalog.to_string().contains("fixture-auth-canary"));
            catalog["git"]["operations"].clone()
        };
        let reads = json!(["git_status", "git_diff", "git_log", "git_branch"]);
        let with_push = json!([
            "git_status",
            "git_diff",
            "git_log",
            "git_branch",
            "git_push"
        ]);
        runtime.host_authentication = std::sync::Arc::new(Authentication::admit(&runtime.config));
        assert_eq!(operations(&runtime), reads);
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        for path in [home.join(".gitconfig"), home.join(".config/git/config")] {
            private_file(
                &path,
                b"[credential]\n helper = store\n username = fixture-auth-canary\n",
                0o600,
            )
            .unwrap();
            assert_eq!(
                operations(&runtime),
                reads,
                "catalog does not rediscover credentials"
            );
            runtime.host_authentication =
                std::sync::Arc::new(Authentication::admit(&runtime.config));
            assert_eq!(operations(&runtime), with_push);
            std::fs::remove_file(path).unwrap();
            assert_eq!(
                operations(&runtime),
                with_push,
                "admitted snapshot survives file removal"
            );
            runtime.host_authentication =
                std::sync::Arc::new(Authentication::admit(&runtime.config));
            assert_eq!(operations(&runtime), reads);
        }
        std::fs::create_dir(home.join(".config/gh")).unwrap();
        private_file(
            &home.join(".config/gh/hosts.yml"),
            b"github.com:\n oauth_token: fixture-auth-canary\n",
            0o600,
        )
        .unwrap();
        runtime.host_authentication = std::sync::Arc::new(Authentication::admit(&runtime.config));
        assert_eq!(
            operations(&runtime),
            reads,
            "PR creation also needs a native gh pin"
        );
        runtime.config.commands.push(crate::tools::config::Program {
            name: "gh".into(),
            executable: "/usr/bin/true".into(),
            sha256: "0".repeat(64),
            interpreter: false,
            inputs: vec![],
        });
        assert_eq!(
            operations(&runtime),
            json!([
                "git_status",
                "git_diff",
                "git_log",
                "git_branch",
                "gh_pr_create"
            ])
        );
        runtime.config.write = false;
        assert_eq!(operations(&runtime), reads);
    }

    #[tokio::test]
    async fn host_credentials_preserve_git_order_resets_scopes_and_includes_refuse() {
        use crate::test_support::process::BoundedOutput;
        let git = parser_git();
        for (configuration, expected) in [
            (
                "[credential \"https://github.com\"]\n helper = \"!f() { echo username=X; echo password=synthetic; }; f\"\n[credential]\n helper =\n",
                None,
            ),
            (
                "[credential]\n helper = \"!f() { echo username=G; echo password=synthetic; }; f\"\n[credential \"https://github.com\"]\n helper = \"!f() { echo username=S; echo password=synthetic; }; f\"\n",
                Some("username=G"),
            ),
            (
                "[credential]\n helper = \"!f() { echo username=G; }; f\"\n[credential \"https://github.com\"]\n helper = \"!f() { echo password=S; }; f\"\n",
                Some("password=S"),
            ),
            (
                "[credential]\n helper=\n username=pinned-user\n useHttpPath=true\n helper=\"!f() { cat > credential-input; echo password=synthetic; }; f\" # comment\n",
                Some("username=pinned-user"),
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let entries =
                credential_config(&git, &[configuration.as_bytes().to_vec()], root.path())
                    .await
                    .unwrap();
            let mut command = Command::new(&git.executable);
            command
                .args(["credential", "fill"])
                .env_clear()
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_DIR", root.path().join(".git"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .current_dir(root.path());
            apply_credentials(&mut command, entries);
            let mut command = command.into_std();
            let input = root.path().join("credential-request");
            std::fs::write(&input, b"protocol=https\nhost=github.com\npath=o/r\n\n").unwrap();
            command.stdin(std::fs::File::open(input).unwrap());
            let output = command.bounded_output_with_input().unwrap();
            assert_eq!(output.status.success(), expected.is_some());
            if let Some(expected) = expected {
                assert!(String::from_utf8(output.stdout).unwrap().contains(expected));
            }
            if root.path().join("credential-input").exists() {
                let input = std::fs::read_to_string(root.path().join("credential-input")).unwrap();
                assert!(input.contains("path=o/r") && input.contains("username=pinned-user"));
            }
        }
        for directive in [
            "[include]\n path=absent",
            "[includeIf \"gitdir:/never\"]\n path=absent",
        ] {
            let root = tempfile::tempdir().unwrap();
            let config = format!("[credential]\n helper=store\n{directive}\n");
            assert!(matches!(
                credential_config(&git, &[config.into_bytes()], root.path()).await,
                Err(ToolError::CredentialIncludes)
            ));
        }
        let root = tempfile::tempdir().unwrap();
        let entries = credential_config(
            &git,
            &[
                b"[credential]\n helper=first\n".to_vec(),
                b"[credential]\n helper=\n helper=\"!printf \\\"quoted\\\"\" # comment\n".to_vec(),
            ],
            root.path(),
        )
        .await
        .unwrap();
        assert_eq!(
            entries,
            vec![
                ("credential.helper".into(), "first".into()),
                ("credential.helper".into(), "".into()),
                ("credential.helper".into(), "!printf \"quoted\"".into())
            ]
        );
    }

    #[tokio::test]
    async fn host_credential_query_is_bounded_and_ignores_parent_repository() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(parent.path().join(".git/objects")).unwrap();
        std::fs::create_dir(parent.path().join(".git/refs")).unwrap();
        std::fs::write(parent.path().join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(
            parent.path().join(".git/config"),
            b"invalid repository config",
        )
        .unwrap();
        let scratch = parent.path().join("scratch");
        private_directory(&scratch).unwrap();
        let git = parser_git();
        assert!(
            credential_config(
                &git,
                &[b"[credential]\nhelper=store\nuseHttpPath\n".to_vec()],
                &scratch
            )
            .await
            .is_ok()
        );
        let second = parent.path().join("second");
        private_directory(&second).unwrap();
        assert_eq!(
            credential_config(
                &git,
                &[
                    b"[credential]\nhelper=store\n".to_vec(),
                    b"helper=unscoped\n".to_vec()
                ],
                &second
            )
            .await
            .unwrap(),
            vec![("credential.helper".into(), "store".into())]
        );
        let third = parent.path().join("third");
        private_directory(&third).unwrap();
        let too_many = format!("[credential]\n{}", "helper=store\n".repeat(257));
        assert!(matches!(
            credential_config(&git, &[too_many.into_bytes()], &third).await,
            Err(ToolError::Limit)
        ));
    }

    #[test]
    fn host_backing_recheck_detects_replacement_and_new_alternates() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        let before = backing_identity(root.path()).unwrap();
        std::fs::rename(git.join("objects"), git.join("saved")).unwrap();
        std::fs::create_dir(git.join("objects")).unwrap();
        assert_ne!(backing_identity(root.path()).unwrap(), before);
        std::fs::create_dir(git.join("objects/info")).unwrap();
        std::fs::write(git.join("objects/info/alternates"), b"/elsewhere").unwrap();
        assert!(backing_identity(root.path()).is_err());
    }

    #[test]
    fn host_helper_search_skips_untrusted_directories() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let rejected = root.path().join("group-writable");
        let admitted = root.path().join("private");
        private_directory(&rejected).unwrap();
        private_directory(&admitted).unwrap();
        std::fs::set_permissions(&rejected, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            helper_path(&[rejected.to_str().unwrap(), admitted.to_str().unwrap()]).unwrap(),
            admitted.to_str().unwrap()
        );
        assert!(matches!(
            helper_path(&[rejected.to_str().unwrap()]),
            Err(ToolError::HostHelperSearch)
        ));
    }

    #[test]
    fn host_executable_limit_is_independent_of_snapshot_limit() {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("private-copy");
        std::fs::copy("/usr/bin/true", &source).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(33 * 1024 * 1024)
            .unwrap();
        let mut hash = Sha256::new();
        let mut file = std::fs::File::open(&source).unwrap();
        let mut buffer = [0; 64 * 1024];
        loop {
            let n = file.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        let mut program = parser_git();
        program.executable = source.to_str().unwrap().into();
        program.sha256 = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        program.verify().unwrap();
        verified_copy(&program, &target).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 33 * 1024 * 1024);
        program.sha256 = "0".repeat(64);
        let mismatch = root.path().join("mismatch");
        assert!(matches!(
            verified_copy(&program, &mismatch),
            Err(ToolError::ChangedInput)
        ));
        assert!(!mismatch.exists());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(crate::tools::config::MAX_EXECUTABLE_BYTES + 1)
            .unwrap();
        assert!(matches!(program.verify(), Err(ToolError::Limit)));
        assert!(matches!(
            verified_copy(&program, &mismatch),
            Err(ToolError::Limit)
        ));
        assert!(!mismatch.exists());
    }

    #[tokio::test]
    async fn host_verified_executable_copy_survives_source_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("gh");
        let bytes = std::fs::read("/usr/bin/true").unwrap();
        std::fs::write(&source, &bytes).unwrap();
        let program = crate::tools::config::Program {
            name: "gh".into(),
            executable: source.to_str().unwrap().into(),
            sha256: hex_digest(&bytes),
            interpreter: false,
            inputs: vec![],
        };
        verified_copy(&program, &target).unwrap();
        std::fs::write(source, std::fs::read("/usr/bin/false").unwrap()).unwrap();
        assert!(
            Command::new(&target)
                .env_clear()
                .status()
                .await
                .unwrap()
                .success()
        );
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert!(verified_copy(&program, &root.path().join("changed")).is_err());
    }

    #[test]
    fn host_auth_files_are_private_and_stale_cleanup_preserves_live_leases() {
        if crate::test_process::isolate_test() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let state = crate::store::StateRoot::admit(&home.join("state")).unwrap();
        sweep_stale(&state);
        assert!(!state.path().join("host-scratch").exists());
        let live = Scratch::acquire(&state).unwrap();
        let parent = scratch_parent(&state, false).unwrap();
        let unpublished_lease = slot_lease(&parent.dir, 1).unwrap().unwrap();
        sweep_stale(&state);
        let stale = parent.path.join("slot-1");
        private_directory(&stale).unwrap();
        private_file(&stale.join("credential"), b"synthetic", 0o600).unwrap();
        sweep_stale(&state);
        assert!(
            stale.exists(),
            "sweep cannot race publication after the slot is locked"
        );
        drop(unpublished_lease);
        let gh = live.path().join("gh");
        private_directory(&gh).unwrap();
        private_file(&gh.join("hosts.yml"), b"synthetic", 0o600).unwrap();
        assert!(private_file(&gh.join("hosts.yml"), b"overwrite", 0o600).is_err());
        assert_eq!(
            std::fs::metadata(&gh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(gh.join("hosts.yml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        sweep_stale(&state);
        assert!(live.path().exists());
        assert!(!stale.exists());
        let other: Vec<_> = (1..SCRATCH_SLOTS)
            .map(|_| Scratch::acquire(&state).unwrap())
            .collect();
        assert!(matches!(Scratch::acquire(&state), Err(ToolError::Limit)));
        sweep_stale(&state);
        assert!(other.iter().all(|scratch| scratch.path().exists()));
        drop(other);
        let path = live.path().to_owned();
        drop(live);
        assert!(
            !path.exists(),
            "normal teardown removes scratch before releasing its lease"
        );
        let mut unsafe_permissions = std::fs::metadata(&parent.path).unwrap().permissions();
        unsafe_permissions.set_mode(0o777);
        std::fs::set_permissions(&parent.path, unsafe_permissions).unwrap();
        assert!(Scratch::acquire(&state).is_err());
        std::fs::set_permissions(&parent.path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&parent.path, home.join("moved")).unwrap();
        std::os::unix::fs::symlink(home.join("moved"), &parent.path).unwrap();
        assert!(Scratch::acquire(&state).is_err());
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
        let crate::Event::ToolStarted { intent, .. } = &f.events[f.events.len() - 2] else {
            unreachable!()
        };
        let row = format!(
            "!\t{}\t[rejected] (non-fast-forward)\n",
            intent.host.as_ref().unwrap().refspec
        );
        assert!(non_fast_forward(intent, row.as_bytes()));
        for output in [
            "authentication refused",
            "PR already exists",
            "",
            &format!("{row}{row}"),
            &row.replace("refs/heads/main", "refs/heads/other"),
        ] {
            assert!(!non_fast_forward(intent, output.as_bytes()));
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
            .set_len((super::super::MAX_SNAPSHOT_BYTES + 1) as u64)
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
        let private = Arc::new(tempfile::tempdir().unwrap());
        let config = config();
        let (prepared, digest) = prepare(
            &config,
            &authentication(temp.path()),
            &workspace,
            &intent,
            Arc::clone(&private),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(private.path().join("gh/hosts.yml")).unwrap(),
            authentication(temp.path()).gh_hosts.unwrap()
        );
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
        assert_eq!(std::fs::read_dir(markers).unwrap().count(), 0);
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
        let refusal_view = Arc::new(tempfile::tempdir().unwrap());
        let (prepared, digest) = prepare(
            &config,
            &authentication(temp.path()),
            &workspace,
            &rejected,
            Arc::clone(&refusal_view),
        )
        .await
        .unwrap();
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
    async fn host_https_helper_credentials_never_enter_provider_journal_or_diagnostics() {
        use crate::test_support::process::BoundedOutput;
        const TEST: &str = "tools::git::host::tests::host_https_helper_credentials_never_enter_provider_journal_or_diagnostics";
        const CANARY: &str = "synthetic-https-credential-canary";
        if std::env::var_os("ARANY_TEST_HOST_HTTPS").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env("ARANY_TEST_HOST_HTTPS", "1")
                .env(
                    "TMPDIR",
                    std::fs::canonicalize(std::env::temp_dir()).unwrap(),
                )
                .args(["--exact", TEST, "--nocapture"])
                .bounded_output_for(Duration::from_secs(30), 64 * 1024)
                .unwrap();
            assert!(
                !output
                    .stdout
                    .windows(CANARY.len())
                    .any(|w| w == CANARY.as_bytes())
            );
            assert!(
                !output
                    .stderr
                    .windows(CANARY.len())
                    .any(|w| w == CANARY.as_bytes())
            );
            assert!(
                output.status.success(),
                "isolated HTTPS credential journey failed: {}",
                if output
                    .stderr
                    .windows(b"pinned git".len())
                    .any(|w| w == b"pinned git")
                {
                    "Git fixture admission refused"
                } else if output
                    .stderr
                    .windows(b"StateNotPrivate".len())
                    .any(|w| w == b"StateNotPrivate")
                {
                    "private State admission refused"
                } else {
                    "credential journey assertion failed"
                }
            );
            crate::test_support::process::assert_single_test_completed(&output.stdout, TEST);
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let private = Arc::new(tempfile::tempdir().unwrap());
        let config = config();
        let mut authentication = authentication(temp.path());
        authentication.git_config = vec![format!("[credential \"https://github.com\"]\n helper =\n helper = \"!f() {{ echo username=synthetic; echo password={CANARY}; }}; f\"\n").into_bytes()];
        let mut f = crate::session::host_protocol_fixture();
        let mut approved = intent(&workspace, push());
        approved.run_id = f.run;
        approved.agent_run_id = f.primary;
        let end = f.events.len();
        for event in &mut f.events[end - 2..] {
            match event {
                crate::Event::ToolStarted { intent, .. } => *intent = approved.clone(),
                crate::Event::ToolPermissionDecided { verdict, .. } => {
                    verdict.intent_digest = approved.digest()
                }
                _ => {}
            }
        }
        let state = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
        crate::enable_development_diagnostics(&state);
        let store = crate::Store::open(state.try_clone().unwrap()).unwrap();
        for event in &f.events {
            store.append(f.session, event.clone()).await.unwrap();
        }
        let (mut command, digest) = prepare(
            &config,
            &authentication,
            &workspace,
            &approved,
            Arc::clone(&private),
        )
        .await
        .unwrap();
        assert!(
            !command
                .as_std()
                .get_args()
                .any(|s| s.to_string_lossy().contains(CANARY))
        );
        let peers = private.path().join("peers");
        private_directory(&peers).unwrap();
        let script = r#"#!/bin/sh
credential=$(printf 'protocol=https\nhost=github.com\npath=example/project.git\n\n' | git credential fill) || exit 9
case "$credential" in *'password=synthetic-https-credential-canary'*) ;; *) exit 8;; esac
printf '%s' "$credential" > credential-flow
printf '%s' "$credential" >&2
while IFS= read -r line; do
 case "$line" in
 capabilities) printf 'push\n\n';;
 'list for-push') printf '\n';;
 push\ *) printf once >> remote-effect; printf 'ok refs/heads/main\n\n';;
 esac
done
"#;
        private_file(&peers.join("git-remote-https"), script.as_bytes(), 0o700).unwrap();
        // Only the test substitutes the transport implementation; argv, credentials and supervisor are production paths.
        command.env("GIT_EXEC_PATH", &peers);
        let observation = supervise(command, approved, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(
            std::fs::read(private.path().join("credential-flow"))
                .unwrap()
                .windows(CANARY.len())
                .any(|w| w == CANARY.as_bytes())
        );
        assert_eq!(
            std::fs::read(private.path().join("remote-effect")).unwrap(),
            b"once"
        );
        let event = crate::Event::ToolFinished {
            checkpoint: None,
            run_id: f.run,
            agent_run_id: f.primary,
            observation: observation.clone(),
        };
        store.append(f.session, event.clone()).await.unwrap();
        let mut failed = observation.clone();
        failed.disposition = ToolDisposition::Uncertain;
        crate::diagnostics::event_failure(&crate::Event::ToolFinished {
            checkpoint: None,
            run_id: f.run,
            agent_run_id: f.primary,
            observation: failed,
        });
        store.close().await.unwrap();
        let reopened = crate::Store::open(state.try_clone().unwrap()).unwrap();
        let events = reopened.load_session(f.session).await.unwrap();
        assert_eq!(
            reopened.load_view(f.session).await.unwrap().unwrap().runs[0]
                .tools
                .iter()
                .filter(|t| t.intent.call.is_host())
                .count(),
            1
        );
        let mut surfaces = vec![
            crate::tools::types::model_observation(&observation).to_string(),
            observation.output.clone(),
        ];
        surfaces.extend(events.iter().map(|e| e.event.payload().unwrap()));
        let diagnostics = std::fs::read_to_string(state.path().join("development.log"));
        if cfg!(debug_assertions) {
            let diagnostics = diagnostics.unwrap();
            assert!(diagnostics.contains(
                "tool operation=git_push disposition=Uncertain receipt=true stops_run=true"
            ));
            surfaces.push(diagnostics);
        } else {
            assert!(
                matches!(diagnostics, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        assert!(surfaces.iter().all(|s| !s.contains(CANARY)));
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn host_preparation_keeps_scratch_alive_off_executor_after_cancellation_or_timeout() {
        struct GatedRoot {
            directory: tempfile::TempDir,
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            dropped: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl AsRef<Path> for GatedRoot {
            fn as_ref(&self) -> &Path {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                self.directory.path()
            }
        }
        impl Drop for GatedRoot {
            fn drop(&mut self) {
                let _ = self.dropped.take().unwrap().send(());
            }
        }
        for expire in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = fixture(temp.path());
            let intent = intent(&workspace, pr());
            let authentication = authentication(temp.path());
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (release, receiver) = std::sync::mpsc::channel();
            let (dropped, finished) = tokio::sync::oneshot::channel();
            let root = Arc::new(GatedRoot {
                directory: tempfile::tempdir().unwrap(),
                entered: std::sync::Mutex::new(Some(entered)),
                release: std::sync::Mutex::new(receiver),
                dropped: Some(dropped),
            });
            let weak = Arc::downgrade(&root);
            let approved = intent.clone();
            let runtime = Duration::from_millis(u64::from(approved.effective_timeout_ms()));
            let deadline = tokio::time::Instant::now() + runtime;
            let task = tokio::spawn(async move {
                tokio::time::timeout_at(
                    deadline,
                    prepare(&config(), &authentication, &workspace, &intent, root),
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(10), ready)
                .await
                .unwrap()
                .unwrap();
            if expire {
                tokio::time::pause();
                tokio::time::advance(runtime).await;
                tokio::time::resume();
                assert!(task.await.unwrap().is_err());
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "printf dispatched > effect"])
                    .current_dir(temp.path());
                let denied = super::supervise(
                    deadline,
                    command,
                    approved,
                    "0".repeat(64),
                    Cancellation::new(),
                )
                .await;
                assert_eq!(denied.disposition, ToolDisposition::Denied);
                assert_eq!(denied.output, PREPARATION_TIMEOUT);
                assert!(denied.attestation.is_none());
                assert!(!temp.path().join("effect").exists());
            } else {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            assert!(
                weak.upgrade().is_some(),
                "blocking preparation owns scratch"
            );
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(10), finished)
                .await
                .unwrap()
                .unwrap();
            assert!(
                weak.upgrade().is_none(),
                "scratch released after preparation"
            );
        }
    }

    #[tokio::test]
    async fn host_git_fake_gh_pins_payload_and_discards_credential_output() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let private = Arc::new(tempfile::tempdir().unwrap());
        let intent = intent(&workspace, pr());
        let (prepared, digest) = prepare(
            &config(),
            &authentication(temp.path()),
            &workspace,
            &intent,
            Arc::clone(&private),
        )
        .await
        .unwrap();
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
        assert!(!private.path().join("gh/config.yml").exists());
        assert_eq!(
            std::fs::read_dir(private.path().join(".git/objects"))
                .unwrap()
                .count(),
            0
        );
        let script = "test \"$1 $2 $3 $4 $5 $6 $7 $8\" = 'pr create --repo github.com/example/project --base base --head main' || exit 7; printf credential-canary; printf credential-canary >&2; printf done > remote-effect";
        let mut fake = vec!["-c".into(), script.into(), "gh".into()];
        fake.extend(args);
        let command = synthetic_peer(&prepared, Path::new("/bin/sh"), &fake);
        let observation = supervise(command, intent, digest, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.valid());
        assert!(private.path().join("remote-effect").is_file());
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
    async fn host_git_lost_ack_and_output_overflow_are_uncertain_without_retry() {
        for script in [
            "printf once >> remote-effect; exit 7",
            "printf once >> remote-effect; printf '%262145s' ''",
            "printf once >> remote-effect; printf '%4097s' '' >&2",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = fixture(temp.path());
            let private = Arc::new(tempfile::tempdir().unwrap());
            let intent = intent(&workspace, pr());
            let (prepared, digest) = prepare(
                &config(),
                &authentication(temp.path()),
                &workspace,
                &intent,
                Arc::clone(&private),
            )
            .await
            .unwrap();
            let command = synthetic_peer(
                &prepared,
                Path::new("/bin/sh"),
                &["-c".into(), script.into()],
            );
            let observation = supervise(command, intent, digest, Cancellation::new()).await;
            assert_eq!(observation.disposition, ToolDisposition::Uncertain);
            assert!(observation.valid() && observation.stops_run());
            assert_eq!(
                std::fs::read(private.path().join("remote-effect")).unwrap(),
                b"once"
            );
        }
    }

    #[tokio::test]
    async fn host_git_supervision_cancellation_timeout_and_expired_approval() {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("effect");
            let intent = EffectIntent {
                host: Some(HostIntent {
                    source_oid: "1".repeat(40),
                    refspec: format!("{}:refs/heads/main", "1".repeat(40)),
                }),
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
                expires_at_ms: crate::tools::now_ms() + 60_000,
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
            let mut expired = intent;
            expired.expires_at_ms = 1;
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf twice >> effect"])
                .current_dir(root.path());
            let denied = supervise(command, expired, "0".repeat(64), Cancellation::new()).await;
            assert_eq!(denied.disposition, ToolDisposition::Denied);
            assert!(denied.attestation.is_none());
            assert_eq!(std::fs::read(marker).unwrap(), b"once");
        }
    }

    #[test]
    fn host_git_private_view_refuses_source_drift_and_symlinked_refs() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = fixture(temp.path());
        let intent = intent(&workspace, push());
        std::fs::write(
            workspace.join(".git/refs/heads/main"),
            format!("{}\n", "0".repeat(40)),
        )
        .unwrap();
        let private = tempfile::tempdir().unwrap();
        assert!(private_view(&workspace, private.path(), &intent).is_err());
        std::os::unix::fs::symlink(temp.path(), workspace.join(".git/refs/heads/escape")).unwrap();
        let call = ToolCall::GitPush {
            remote: "https://host/o/r".into(),
            branch: "escape/main".into(),
        };
        assert!(pin(&workspace, &call).is_err());
    }
}

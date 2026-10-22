use super::ToolError;
use super::cancel::Cancellation;
use super::config::Program;
use super::fs::{identity, open_directory, open_file};
use super::guard::reap_owned_child;
use super::output::{Window, read_window_into};
use super::toolchains::RuntimeGrant;
use super::types::{CommandChangeset, LimitCause, ToolDisposition};
use crate::session::WorkspaceIdentity;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub(crate) const PRIVILEGE_TOOLS: &[&str] = &[
    "sudo",
    "sudoedit",
    "doas",
    "su",
    "pkexec",
    "run0",
    "runuser",
    "chroot",
    "systemd-run",
    "machinectl",
    "pfexec",
    "ksu",
    "dzdo",
    "authopen",
];
const WRAPPERS: &[&str] = &[
    "env",
    "xargs",
    "find",
    "nice",
    "ionice",
    "timeout",
    "nohup",
    "setsid",
    "time",
    "busybox",
    "toybox",
    "stdbuf",
    "chrt",
    "taskset",
    "flock",
    "command",
    "exec",
    "strace",
    "ltrace",
    "nsenter",
    "unshare",
    "setpriv",
    "script",
    "watch",
    "eatmydata",
    "runcon",
    "capsh",
    "firejail",
    "bwrap",
];
const INTERPRETERS: &[&str] = &[
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "mksh",
    "fish",
    "csh",
    "tcsh",
    "python",
    "python2",
    "python3",
    "pypy",
    "pypy3",
    "node",
    "nodejs",
    "deno",
    "bun",
    "perl",
    "ruby",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "pwsh",
    "powershell",
];
pub(crate) const HOST_POLICY_VERSION: u32 = 1;

fn basename(program: &str) -> Option<&str> {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
}

pub(crate) fn interpreter(program: &str) -> bool {
    basename(program)
        .is_some_and(|name| INTERPRETERS.contains(&name) || ["busybox", "toybox"].contains(&name))
}

pub(crate) fn wrapper(program: &str) -> bool {
    basename(program).is_some_and(|name| WRAPPERS.contains(&name))
}

pub(super) fn wrapper_interpreters_admitted(
    intent: &super::EffectIntent,
    commands: &[Program],
) -> bool {
    let super::ToolCall::CommandUnguarded { args, cwd, .. } = &intent.call else {
        return false;
    };
    let Some(host) = &intent.host_command else {
        return false;
    };
    let workspace = Path::new(&host.workspace);
    let cwd = workspace.join(cwd);
    let mut checked = std::collections::BTreeSet::new();
    args.iter().all(|arg| {
        let candidate = arg.split_once('=').map_or(arg.as_str(), |(_, value)| value);
        let resolved = if candidate.contains('/') {
            cwd.join(candidate).canonicalize().ok()
        } else {
            use std::os::unix::fs::MetadataExt;
            std::env::split_paths(&host.path)
                .map(|entry| entry.join(candidate))
                .find(|path| {
                    path.metadata()
                        .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
                })
                .and_then(|path| path.canonicalize().ok())
        };
        let candidate = if interpreter(candidate) {
            candidate
        } else if let Some(target) = resolved
            .as_ref()
            .and_then(|target| target.to_str())
            .filter(|target| interpreter(target))
        {
            target
        } else {
            return true;
        };
        if !checked.insert(candidate.to_owned()) {
            return true;
        }
        pin(candidate, workspace, &host.path).is_ok_and(|pin| {
            commands.iter().any(|command| {
                command.interpreter
                    && command.executable == pin.executable
                    && command.sha256 == pin.sha256
            })
        })
    })
}

fn administrator(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg.to_ascii_lowercase()
            .contains("with administrator privileges")
    })
}

fn privileged(name: &str, administrator: bool) -> bool {
    PRIVILEGE_TOOLS.contains(&name) || name == "osascript" && administrator
}

fn lexical_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ';' | '&' | '|' | '(' | ')' | '`' | '$' | '\'' | '"' | '<' | '>' | '='
            )
    })
    .filter(|token| !token.is_empty())
}

fn inline_option(arg: &str) -> bool {
    let option = arg.split('=').next().unwrap_or(arg);
    [
        "-c",
        "-e",
        "-command",
        "-commandwithargs",
        "--command",
        "--eval",
        "--execute",
        "--expression",
    ]
    .iter()
    .any(|name| option.eq_ignore_ascii_case(name))
        || arg.starts_with('-')
            && !arg.starts_with("--")
            && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
            && arg.contains('c')
}

pub(crate) fn prohibited(program: &str, args: &[String]) -> bool {
    let Some(name) = basename(program) else {
        return false;
    };
    let administrator = administrator(args);
    let wrapper = WRAPPERS.contains(&name);
    let env = name == "env" || wrapper && args.iter().any(|arg| basename(arg) == Some("env"));
    privileged(name, administrator)
        || env
            && args.iter().any(|arg| {
                arg == "--split-string"
                    || arg.starts_with("--split-string=")
                    || arg.starts_with('-') && !arg.starts_with("--") && arg.contains('S')
            })
        || wrapper
            && args
                .iter()
                .filter_map(|arg| basename(arg))
                .any(|name| privileged(name, administrator))
        || (interpreter(program) || wrapper || args.iter().any(|arg| inline_option(arg)))
            && args
                .iter()
                .flat_map(|arg| lexical_tokens(arg))
                .filter_map(basename)
                .any(|name| privileged(name, administrator))
}

pub(crate) fn prohibited_intent(intent: &super::EffectIntent) -> bool {
    let super::ToolCall::CommandUnguarded {
        program, args, cwd, ..
    } = &intent.call
    else {
        return false;
    };
    prohibited(program, args)
        || intent.host_command.as_ref().is_some_and(|pin| {
            let cwd = Path::new(&pin.workspace).join(cwd);
            live_prohibited(program, args, &cwd, &pin.path)
                || live_prohibited(&pin.executable, args, &cwd, &pin.path)
        })
}

fn live_prohibited(program: &str, args: &[String], cwd: &Path, path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    if prohibited(program, args) {
        return true;
    }
    if !basename(program).is_some_and(|name| WRAPPERS.contains(&name)) {
        return false;
    }
    let mut identities = Vec::new();
    for arg in args
        .iter()
        .flat_map(|arg| {
            std::iter::once(arg.as_str()).chain(arg.split_once('=').map(|(_, value)| value))
        })
        .filter(|arg| arg.contains('/'))
    {
        let Ok(resolved) = cwd.join(arg).canonicalize() else {
            continue;
        };
        if resolved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| prohibited(name, args))
        {
            return true;
        }
        if let Ok(metadata) = resolved.metadata() {
            identities.push((metadata.dev(), metadata.ino()));
        }
    }
    if identities.is_empty() {
        return false;
    }
    if path.len() > 16 * 1024 {
        return true;
    }
    let administrator = administrator(args);
    PRIVILEGE_TOOLS
        .iter()
        .copied()
        .chain(std::iter::once("osascript"))
        .filter(|name| privileged(name, administrator))
        .any(|name| {
            std::env::split_paths(path)
                .filter(|entry| entry.is_absolute())
                .filter_map(|entry| entry.join(name).metadata().ok())
                .find(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
                .is_some_and(|metadata| identities.contains(&(metadata.dev(), metadata.ino())))
        })
}

pub(crate) fn manual_handoff(call: &super::ToolCall) -> String {
    let super::ToolCall::CommandUnguarded {
        program, args, cwd, ..
    } = call
    else {
        return "Host command refused; nothing ran".into();
    };
    let argv: Vec<_> = std::iter::once(program).chain(args).collect();
    format!(
        "Arany cannot run privilege-escalation commands. Please run this command by hand:\nargv (JSON): {}\nWorkspace-relative cwd (JSON): {}\nNothing ran. Do not repackage this command through a wrapper, shell or child.",
        serde_json::to_string(&argv).expect("command argv"),
        serde_json::to_string(cwd).expect("cwd")
    )
}

impl super::types::HostCommandPin {
    pub(crate) fn valid(&self) -> bool {
        Path::new(&self.workspace).is_absolute()
            && self.workspace.len() <= 4096
            && self
                .state_roots
                .iter()
                .all(|root| Path::new(root).is_absolute() && root.len() <= 4096)
            && Path::new(&self.executable).is_absolute()
            && self.executable.len() <= 4096
            && self.inode != 0
            && super::types::valid_digest(&self.sha256)
            && !self.path.is_empty()
            && self.path.len() <= 16 * 1024
            && std::env::split_paths(&self.path).all(|path| path.is_absolute())
            && self.environment_digest != [0; 32]
    }

    pub(crate) fn verify(&self, workspace: &Path) -> Result<(), ToolError> {
        let mut current = pin(&self.executable, workspace, &self.path)?;
        current.state_roots.clone_from(&self.state_roots);
        current.permissions_digest = self.permissions_digest;
        if &current != self {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }
}

pub(crate) fn sensitive(call: &super::ToolCall, pin: &super::types::HostCommandPin) -> bool {
    let super::ToolCall::CommandUnguarded { args, .. } = call else {
        return false;
    };
    state_target(call, pin)
        || args.iter().any(|arg| {
            arg.split(|c: char| !c.is_alphanumeric() && !matches!(c, '.' | '_' | '-'))
                .any(|part| {
                    part == ".env"
                        || part.starts_with(".env.")
                        || [".ssh", ".aws", ".gnupg"].contains(&part)
                })
        })
}

pub(crate) fn state_target(call: &super::ToolCall, pin: &super::types::HostCommandPin) -> bool {
    let super::ToolCall::CommandUnguarded { args, cwd, .. } = call else {
        return false;
    };
    pin.state_roots.iter().any(|root| {
        root.is_empty()
            || args.iter().any(|arg| {
                arg.contains(root)
                    || arg
                        .split(|c: char| c.is_whitespace() || matches!(c, '=' | '\'' | '"' | ';'))
                        .any(|token| {
                            let path = Path::new(&pin.workspace).join(cwd).join(token);
                            let mut normalized = std::path::PathBuf::new();
                            for part in path.components() {
                                match part {
                                    std::path::Component::ParentDir => {
                                        normalized.pop();
                                    }
                                    std::path::Component::CurDir => {}
                                    part => normalized.push(part.as_os_str()),
                                }
                            }
                            normalized.starts_with(root)
                        })
            })
    })
}

pub(crate) fn same_approval(prior: &super::EffectIntent, next: &super::EffectIntent) -> bool {
    let (
        super::ToolCall::CommandUnguarded {
            program: p,
            args: a,
            cwd: c,
            ..
        },
        super::ToolCall::CommandUnguarded {
            program, args, cwd, ..
        },
    ) = (&prior.call, &next.call)
    else {
        return false;
    };
    let left = &prior.limits;
    let right = &next.limits;
    p == program
        && a == args
        && c == cwd
        && prior.host_command.is_some()
        && prior.host_command == next.host_command
        && prior.workspace_device == next.workspace_device
        && prior.workspace_inode == next.workspace_inode
        && prior.policy_digest == next.policy_digest
        && prior.enforcement_digest == next.enforcement_digest
        && left.network == right.network
        && left.resource_class == right.resource_class
        && left.resource_profile == right.resource_profile
        && right.runtime_ms <= left.runtime_ms
        && right.result_bytes <= left.result_bytes
        && right.cache_transfer_ms <= left.cache_transfer_ms
        && right.memory_bytes <= left.memory_bytes
        && right.max_processes <= left.max_processes
        && right.scratch_bytes <= left.scratch_bytes
        && right.project_bytes <= left.project_bytes
        && right.cache_requested_bytes <= left.cache_requested_bytes
        && right.cache_bytes <= left.cache_bytes
        && right.cache_candidate_bytes <= left.cache_candidate_bytes
}

pub(crate) fn environment_digest(path: &str) -> [u8; 32] {
    let values = ["HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"].map(|name| {
        (
            name,
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
        )
    });
    super::types::digest(
        &serde_json::to_vec(&(HOST_POLICY_VERSION, path, values)).expect("host environment"),
    )
}

pub(crate) fn pin(
    program: &str,
    workspace: &Path,
    path: &str,
) -> Result<super::types::HostCommandPin, ToolError> {
    use std::os::unix::fs::MetadataExt;
    if path.is_empty() || path.len() > 16 * 1024 {
        return Err(ToolError::Configuration);
    }
    let workspace = workspace.canonicalize().map_err(|_| ToolError::Path)?;
    let paths: Vec<_> = std::env::split_paths(path).collect();
    if paths.iter().any(|entry| {
        !entry.is_absolute()
            || entry.starts_with(&workspace)
            || entry
                .canonicalize()
                .is_ok_and(|entry| entry.starts_with(&workspace))
    }) {
        return Err(ToolError::Configuration);
    }
    let requested = Path::new(program);
    if !requested.is_absolute() && requested.components().count() != 1 {
        return Err(ToolError::Configuration);
    }
    let name = requested.file_name().ok_or(ToolError::Configuration)?;
    let executable = paths
        .iter()
        .map(|entry| entry.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
        .ok_or(ToolError::Configuration)?
        .canonicalize()
        .map_err(|_| ToolError::Configuration)?;
    if requested.is_absolute()
        && requested
            .canonicalize()
            .map_err(|_| ToolError::Configuration)?
            != executable
    {
        return Err(ToolError::Configuration);
    }
    if executable.starts_with(&workspace) {
        return Err(ToolError::Configuration);
    }
    let file = super::fs::open_absolute(executable.to_str().ok_or(ToolError::Path)?)?;
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(ToolError::Configuration);
    }
    let bytes = super::fs::read_regular(file, super::types::MAX_SNAPSHOT_BYTES)?;
    if !super::executable::has_magic(&bytes, super::executable::EXECUTABLES.program_magics) {
        return Err(ToolError::Configuration);
    }
    Ok(super::types::HostCommandPin {
        state_roots: [String::new(), String::new()],
        permissions_digest: None,
        workspace: workspace.to_str().ok_or(ToolError::Path)?.to_owned(),
        executable: executable.to_str().ok_or(ToolError::Path)?.to_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        sha256: super::types::hex_digest(&bytes),
        path: path.into(),
        environment_digest: environment_digest(path),
    })
}

pub fn tool_host_main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(2);
    let Some(executable) = args.next().and_then(|arg| arg.into_string().ok()) else {
        return std::process::ExitCode::FAILURE;
    };
    let Ok(args) = args
        .take(65)
        .map(std::ffi::OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
    else {
        return std::process::ExitCode::FAILURE;
    };
    host_exec(&executable, &args)
}

fn host_exec(executable: &str, args: &[String]) -> std::process::ExitCode {
    use std::os::unix::process::CommandExt;
    let resolved = Path::new(executable).canonicalize().ok();
    if !Path::new(executable).is_absolute()
        || executable.len() > 4096
        || !super::process::valid_command("host", args, "")
        || prohibited(executable, args)
        || !resolved
            .as_ref()
            .and_then(|path| path.to_str())
            .is_some_and(|program| {
                std::env::current_dir().is_ok_and(|cwd| {
                    !live_prohibited(
                        program,
                        args,
                        &cwd,
                        &std::env::var("PATH").unwrap_or_default(),
                    )
                })
            })
        || rustix::process::setsid().is_err()
    {
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(test)]
    {
        assert_eq!(
            rustix::process::getsid(None).unwrap(),
            rustix::process::getpid()
        );
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_err()
        );
    }
    let mut command = std::process::Command::new(executable);
    command.args(args);
    #[cfg(test)]
    command.env_remove("ARANY_TEST_HOST_ARGS");
    let _error = command.exec();
    std::process::ExitCode::FAILURE
}

const UNCONFIRMED: &str =
    "Unguarded run outcome unconfirmed; processes may still run. Do not retry automatically.";
const REAP_BOUND: Duration = Duration::from_secs(1);
const GROUP_POLL: Duration = Duration::from_millis(10);
const GROUP_POLLS: u32 = 100;

pub(super) struct Request<'a> {
    pub(super) launcher: &'a Path,
    pub(super) program: &'a Program,
    pub(super) host: Option<&'a super::types::HostCommandPin>,
    pub(super) args: &'a [String],
    pub(super) cwd: &'a str,
    pub(super) workspace: &'a Path,
    pub(super) workspace_identity: WorkspaceIdentity,
    pub(super) runtime_grants: &'a [RuntimeGrant],
    pub(super) result_bytes: u32,
    pub(super) runtime_ms: u32,
}

pub(super) struct Outcome {
    pub(super) disposition: ToolDisposition,
    pub(super) limit_cause: Option<LimitCause>,
    pub(super) output: String,
    pub(super) spawned: bool,
    pub(super) descendants_stopped: bool,
    pub(super) pid: u32,
    pub(super) exit_code: Option<i32>,
}

impl Outcome {
    fn unstarted(disposition: ToolDisposition, output: &str) -> Self {
        Self {
            disposition,
            limit_cause: (disposition == ToolDisposition::Limit).then_some(LimitCause::Resource),
            output: output.into(),
            spawned: false,
            descendants_stopped: false,
            pid: 0,
            exit_code: None,
        }
    }

    fn started(disposition: ToolDisposition, output: &str, descendants_stopped: bool) -> Self {
        Self {
            disposition,
            limit_cause: (disposition == ToolDisposition::Limit).then_some(LimitCause::Resource),
            output: output.into(),
            spawned: true,
            descendants_stopped,
            pid: 0,
            exit_code: None,
        }
    }
}

enum Ended {
    Done(std::process::ExitStatus, Window, Window, usize),
    Timeout(Window, Window, usize),
    Cancelled,
    Failed(ToolError),
}

pub(super) async fn run(request: Request<'_>, mut cancellation: Cancellation) -> Outcome {
    if cancellation.is_cancelled() {
        return Outcome::unstarted(
            ToolDisposition::Cancelled,
            "Cancelled before the unguarded run started; nothing ran",
        );
    }
    let launcher = request.launcher.to_owned();
    let program = request.program.clone();
    let host = request.host.cloned();
    let args = request.args.to_vec();
    let cwd = request.cwd.to_owned();
    let workspace = request.workspace.to_owned();
    let runtime_grants = request.runtime_grants.to_vec();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare(&Request {
            launcher: &launcher,
            program: &program,
            host: host.as_ref(),
            args: &args,
            cwd: &cwd,
            workspace: &workspace,
            workspace_identity: request.workspace_identity,
            runtime_grants: &runtime_grants,
            result_bytes: request.result_bytes,
            runtime_ms: request.runtime_ms,
        })
    })
    .await
    .unwrap_or(Err(ToolError::Operation));
    if cancellation.is_cancelled() {
        return Outcome::unstarted(
            ToolDisposition::Cancelled,
            "Cancelled before the unguarded run started; nothing ran",
        );
    }
    let (mut command, cap) = match prepared {
        Ok(prepared) => prepared,
        Err(ToolError::Path) => {
            return Outcome::unstarted(
                ToolDisposition::Denied,
                "Working directory is unavailable or outside the Workspace; nothing ran",
            );
        }
        Err(_) => {
            return Outcome::unstarted(
                ToolDisposition::Failed,
                "Unguarded run pinned input unavailable before dispatch; nothing ran",
            );
        }
    };
    let Ok(mut child) = command.spawn() else {
        return Outcome::unstarted(
            ToolDisposition::Failed,
            "Unguarded run could not start; nothing ran",
        );
    };
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    let ended = wait(
        &mut child,
        cap,
        Duration::from_millis(u64::from(request.runtime_ms)),
        &mut cancellation,
    )
    .await;
    let reaped = match &ended {
        Ended::Done(..) => true,
        _ => {
            if let Some(group) = group {
                let _ = kill_process_group(group, Signal::KILL);
            }
            let _ = child.start_kill();
            tokio::time::timeout(REAP_BOUND, reap_owned_child(&mut child))
                .await
                .is_ok_and(|status| status.is_ok())
        }
    };
    // While a member lives the leader pid stays reserved as the group id; after a normal exit, pid reuse is theoretical.
    let stopped = match group {
        Some(group) => stop_group(group).await,
        None => false,
    };
    let mut outcome = settle(ended, reaped, stopped);
    outcome.pid = group.map_or(0, |group| group.as_raw_nonzero().get().unsigned_abs());
    outcome
}

fn settle(ended: Ended, reaped: bool, stopped: bool) -> Outcome {
    if !reaped || !stopped {
        return Outcome::started(ToolDisposition::Uncertain, UNCONFIRMED, false);
    }
    finish(ended, stopped)
}

fn finish(ended: Ended, stopped: bool) -> Outcome {
    match ended {
        Ended::Cancelled => Outcome::started(
            ToolDisposition::Cancelled,
            "Unguarded run cancelled; host effects may have occurred",
            stopped,
        ),
        Ended::Failed(ToolError::OutputCeiling) => {
            let mut outcome = Outcome::started(
                ToolDisposition::Limit,
                "Output passed 1 GiB; stopped; host effects may have occurred",
                stopped,
            );
            outcome.limit_cause = Some(LimitCause::OutputCeiling);
            outcome
        }
        Ended::Timeout(stdout, stderr, budget) => {
            let output = super::process::fit_command(
                None,
                &stdout,
                &stderr,
                CommandChangeset::default(),
                budget,
                super::process::Truncation::Allow,
            )
            .unwrap_or_else(|_| "Tool runtime expired; host effects may have occurred".into());
            let mut outcome = Outcome::started(ToolDisposition::Limit, &output, stopped);
            outcome.limit_cause = Some(LimitCause::Timeout);
            outcome
        }
        Ended::Failed(ToolError::Limit) => Outcome::started(
            ToolDisposition::Limit,
            "Tool result or resource limit reached; host effects may have occurred",
            stopped,
        ),
        Ended::Failed(_) => Outcome::started(
            ToolDisposition::Failed,
            "Unguarded run failed; host effects may have occurred; no automatic retry",
            stopped,
        ),
        Ended::Done(status, stdout, stderr, budget) => {
            let output = super::process::fit_command(
                status.code(),
                &stdout,
                &stderr,
                CommandChangeset::default(),
                budget,
                super::process::Truncation::Allow,
            )
            .ok();
            let Some(output) = output else {
                return Outcome::started(
                    ToolDisposition::Limit,
                    "Tool result or resource limit reached; host effects may have occurred",
                    stopped,
                );
            };
            let disposition = if status.code() == Some(0) {
                ToolDisposition::Succeeded
            } else {
                ToolDisposition::Failed
            };
            Outcome {
                disposition,
                limit_cause: (disposition == ToolDisposition::Limit)
                    .then_some(LimitCause::Resource),
                output,
                spawned: true,
                descendants_stopped: stopped,
                pid: 0,
                exit_code: status.code(),
            }
        }
    }
}

async fn wait(
    child: &mut tokio::process::Child,
    cap: usize,
    deadline: Duration,
    cancellation: &mut Cancellation,
) -> Ended {
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Ended::Failed(ToolError::Operation);
    };
    let mut stdout = Window::new(cap);
    let mut stderr = Window::new(cap);
    let work = tokio::time::timeout(deadline, async {
        tokio::try_join!(
            async {
                reap_owned_child(child)
                    .await
                    .map_err(|_| ToolError::Operation)
            },
            read_window_into(out, &mut stdout),
            read_window_into(err, &mut stderr)
        )
    });
    let joined = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Ended::Cancelled,
        joined = work => joined,
    };
    match joined {
        Ok(Ok((status, (), ()))) => Ended::Done(status, stdout, stderr, cap),
        Ok(Err(error)) => Ended::Failed(error),
        Err(_) => Ended::Timeout(stdout, stderr, cap),
    }
}

pub(super) async fn stop_group(group: Pid) -> bool {
    let _ = kill_process_group(group, Signal::KILL);
    for _ in 0..GROUP_POLLS {
        if test_kill_process_group(group) == Err(Errno::SRCH) {
            return true;
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
    false
}

fn prepare(request: &Request<'_>) -> Result<(Command, usize), ToolError> {
    request
        .program
        .verify()
        .map_err(|_| ToolError::ChangedInput)?;
    let executable = Path::new(&request.program.executable);
    let cwd_path = request.workspace.join(request.cwd);
    let path = request.host.map_or_else(
        || std::env::var("PATH").unwrap_or_default(),
        |pin| pin.path.clone(),
    );
    if live_prohibited(&request.program.name, request.args, &cwd_path, &path)
        || live_prohibited(&request.program.executable, request.args, &cwd_path, &path)
    {
        return Err(ToolError::Configuration);
    }
    if let Some(pin) = request.host {
        pin.verify(request.workspace)?;
    }
    let workspace = open_directory(request.workspace).map_err(|_| ToolError::ChangedInput)?;
    let found = identity(&workspace).map_err(|_| ToolError::ChangedInput)?;
    if (found.0, found.1)
        != (
            request.workspace_identity.device,
            request.workspace_identity.inode,
        )
    {
        return Err(ToolError::ChangedInput);
    }
    let cwd = if request.cwd.is_empty() {
        "."
    } else {
        request.cwd
    };
    if !open_file(&workspace, cwd)
        .map_err(|_| ToolError::Path)?
        .metadata()
        .map_err(|_| ToolError::Path)?
        .is_dir()
    {
        return Err(ToolError::Path);
    }
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_owned())
        .ok_or(ToolError::Configuration)?;
    let mut command = Command::new(request.launcher);
    command.env_clear();
    #[cfg(not(test))]
    command
        .arg("--internal-host-command")
        .arg(executable)
        .args(request.args);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "tools::unguarded::tests::host_launcher",
            "--nocapture",
            "--quiet",
        ])
        .env(
            "ARANY_TEST_HOST_ARGS",
            serde_json::to_string(&(executable, request.args)).expect("host test args"),
        );
    command
        .current_dir(request.workspace.join(request.cwd))
        .env(
            "PATH",
            match request.host {
                Some(pin) => pin.path.clone(),
                None => super::guard::runtime_path(request.runtime_grants)?,
            },
        )
        .env("HOME", home)
        .env("TMPDIR", std::env::temp_dir())
        .env("NO_COLOR", "1")
        .env("ARANY_TOOL_WORKSPACE", request.workspace)
        .env("ARANY_UNGUARDED", "1");
    for name in ["LANG", "LC_ALL", "TERM"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok((command, request.result_bytes as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::types::hex_digest;
    use std::os::unix::fs::PermissionsExt;

    fn host_call(args: &[&str]) -> crate::tools::ToolCall {
        crate::tools::ToolCall::CommandUnguarded {
            program: "find".into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: "sub".into(),
            justification: "Inspect requested paths".into(),
        }
    }

    #[test]
    fn wrappers_require_the_nested_interpreters_own_path_and_digest() {
        let fixture = fixture();
        let root = fixture._dir.path().canonicalize().unwrap();
        let mut host = pin("sh", &fixture.workspace, root.to_str().unwrap()).unwrap();
        host.executable = "/usr/bin/env".into();
        let mut intent = crate::tools::EffectIntent {
            host: None,
            host_command: Some(host),
            target: crate::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::RunId::new(),
            agent_run_id: crate::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: fixture.identity.device,
            workspace_inode: fixture.identity.inode,
            call: host_call(&[]),
            limits: crate::tools::ToolLimits::default(),
            use_count: 1,
        };
        std::os::unix::fs::symlink(&fixture.program.executable, root.join("alias")).unwrap();
        std::os::unix::fs::symlink(
            &fixture.program.executable,
            fixture.workspace.join("sub/alias"),
        )
        .unwrap();
        for candidate in [
            "sh",
            fixture.program.executable.as_str(),
            "alias",
            "./alias",
        ] {
            intent.call = crate::tools::ToolCall::CommandUnguarded {
                program: "env".into(),
                args: vec![candidate.into(), "-c".into(), "printf inspected".into()],
                cwd: "sub".into(),
                justification: "Inspect through the declared interpreter".into(),
            };
            for (command, expected) in [
                (None, false),
                (Some(fixture.program.clone()), true),
                (
                    Some(Program {
                        interpreter: false,
                        ..fixture.program.clone()
                    }),
                    false,
                ),
                (
                    Some(Program {
                        sha256: "0".repeat(64),
                        ..fixture.program.clone()
                    }),
                    false,
                ),
                (
                    Some(Program {
                        executable: "/usr/bin/env".into(),
                        ..fixture.program.clone()
                    }),
                    false,
                ),
            ] {
                let commands: Vec<_> = command.into_iter().collect();
                assert_eq!(
                    wrapper_interpreters_admitted(&intent, &commands),
                    expected,
                    "{candidate}"
                );
            }
        }
        let crate::tools::ToolCall::CommandUnguarded { args, .. } = &mut intent.call else {
            unreachable!()
        };
        *args = vec!["sh".into()];
        let mut command = fixture.program.clone();
        command.executable = root.join("other/sh").to_string_lossy().into_owned();
        assert!(!wrapper_interpreters_admitted(&intent, &[command]));
    }

    #[test]
    fn interpreter_table_matches_the_closed_policy_without_name_heuristics() {
        let names = [
            "awk",
            "gawk",
            "mawk",
            "nawk",
            "sed",
            "sh",
            "bash",
            "dash",
            "zsh",
            "ksh",
            "mksh",
            "fish",
            "csh",
            "tcsh",
            "python",
            "python2",
            "python3",
            "pypy",
            "pypy3",
            "node",
            "nodejs",
            "deno",
            "bun",
            "perl",
            "ruby",
            "php",
            "lua",
            "luajit",
            "tclsh",
            "wish",
            "pwsh",
            "powershell",
        ];
        assert_eq!(INTERPRETERS, names);
        for program in names.into_iter().chain(["busybox", "toybox"]) {
            assert!(interpreter(program), "{program}");
            assert!(interpreter(&format!("/usr/bin/{program}")), "{program}");
        }
        for program in [
            "find",
            "du",
            "python-helper",
            "node-tool",
            "python3.12",
            "Rscript",
        ] {
            assert!(!interpreter(program), "{program}");
        }
    }

    #[test]
    fn wrappers_never_admit_literal_privilege_tools() {
        let names = [
            "env",
            "xargs",
            "find",
            "nice",
            "ionice",
            "timeout",
            "nohup",
            "setsid",
            "time",
            "busybox",
            "toybox",
            "stdbuf",
            "chrt",
            "taskset",
            "flock",
            "command",
            "exec",
            "strace",
            "ltrace",
            "nsenter",
            "unshare",
            "setpriv",
            "script",
            "watch",
            "eatmydata",
            "runcon",
            "capsh",
            "firejail",
            "bwrap",
        ];
        assert_eq!(WRAPPERS, names);
        for wrapper in WRAPPERS {
            for privilege in PRIVILEGE_TOOLS {
                for token in [privilege.to_string(), format!("/usr/bin/{privilege}")] {
                    let args = vec!["--".into(), "5".into(), token, "true".into()];
                    assert!(prohibited(wrapper, &args), "{wrapper} {privilege}");
                    assert!(prohibited(&format!("/usr/bin/{wrapper}"), &args));
                }
            }
            assert!(!prohibited(wrapper, &["sudo-helper".into()]));
            assert!(prohibited(
                wrapper,
                &[
                    "/usr/bin/osascript".into(),
                    "-e".into(),
                    "do shell script \"id\" with administrator privileges".into()
                ]
            ));
        }
        let args = [
            "-e".into(),
            "do shell script \"id\" with administrator privileges".into(),
        ];
        assert!(prohibited("/usr/bin/osascript", &args));
        assert!(!prohibited(
            "osascript",
            &["-e".into(), "display dialog \"hello\"".into()]
        ));
        assert!(prohibited("sh", &["-c".into(), "sudo true".into()]));
    }

    #[test]
    fn env_split_strings_are_never_admitted() {
        for args in [
            vec!["-S", "sudo id"],
            vec!["--split-string", "sudo id"],
            vec!["--split-string=sudo id"],
            vec!["-S/usr/bin/sudo id"],
            vec!["-iS", "sudo id"],
            vec!["-S", "printf benign"],
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert!(prohibited("env", &args), "{args:?}");
            let nested = std::iter::once("/usr/bin/env".into())
                .chain(args)
                .collect::<Vec<_>>();
            assert!(prohibited("timeout", &nested));
        }
    }

    #[test]
    fn inline_privilege_tokens_are_refused_without_evaluating_payloads() {
        for (program, option) in [
            ("sh", "-c"),
            ("bash", "-lc"),
            ("python3", "-c"),
            ("perl", "-e"),
            ("node", "--eval"),
            ("ruby", "-e"),
            ("pwsh", "-Command"),
            ("custom-interpreter", "--execute"),
            ("python3.12", "-c"),
        ] {
            for privilege in PRIVILEGE_TOOLS {
                for text in [
                    format!("{privilege} true"),
                    format!("true;{privilege} true"),
                    format!("true&&{privilege} true"),
                    format!("true|/usr/bin/{privilege} true"),
                    format!("({privilege} true)"),
                    format!("echo $({privilege} true)"),
                    format!("echo `{privilege} true`"),
                    format!("system(\"{privilege} true\")"),
                    format!("echo\n{privilege}\tid"),
                ] {
                    assert!(
                        prohibited(program, &[option.into(), text.clone()]),
                        "{program} {option} {text}"
                    );
                }
            }
            assert!(!prohibited(
                program,
                &[option.into(), "printf benign".into()]
            ));
            assert!(!prohibited(
                program,
                &[option.into(), "sudo-helper true".into()]
            ));
        }
        assert!(prohibited(
            "env",
            &["sh".into(), "-c".into(), "sudo true".into()]
        ));
        assert!(!prohibited("sh", &["script.sh".into()]));
        assert!(prohibited("sh", &["-c".into(), "su\"\"do true".into()]));
    }

    #[test]
    fn wrapper_paths_resolve_symlinks_and_compare_privilege_identities() {
        let fixture = fixture();
        let root = fixture._dir.path().canonicalize().unwrap();
        let privilege = root.join("sudo");
        std::fs::copy(&fixture.program.executable, &privilege).unwrap();
        let alias = fixture.workspace.join("alias");
        std::os::unix::fs::symlink(&privilege, &alias).unwrap();
        let hardlink = fixture.workspace.join("hardlink");
        std::fs::hard_link(&privilege, &hardlink).unwrap();
        for arg in [
            alias.to_str().unwrap(),
            "./alias",
            hardlink.to_str().unwrap(),
            "./hardlink",
            "--command=./alias",
            "--command=./hardlink",
        ] {
            assert!(
                live_prohibited(
                    "env",
                    &[arg.into()],
                    &fixture.workspace,
                    root.to_str().unwrap()
                ),
                "{arg}"
            );
        }
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&fixture.program.executable, &alias).unwrap();
        assert!(!live_prohibited(
            "env",
            &["./alias".into()],
            &fixture.workspace,
            root.to_str().unwrap()
        ));
        assert!(!live_prohibited(
            "env",
            &["./missing".into()],
            &fixture.workspace,
            root.to_str().unwrap()
        ));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&privilege, &alias).unwrap();
        assert!(live_prohibited(
            "env",
            &["../alias".into()],
            &fixture.workspace.join("sub"),
            root.to_str().unwrap()
        ));
    }

    #[test]
    fn host_paths_warn_and_state_targets_never_match_cached_authority() {
        let pin = crate::tools::types::test_host_pin();
        for (arg, warning, state) in [
            ("/home/user/.env", true, false),
            ("--config=/home/user/.env.local", true, false),
            ("cat '/home/user/.ssh/id_ed25519'", true, false),
            ("/home/user/.aws/config", true, false),
            ("/home/user/.gnupg", true, false),
            ("--output=/state/store.sqlite", true, true),
            ("cat '/account/key'", true, true),
            ("../../state/settings.json", true, true),
            ("/ordinary/file", false, false),
        ] {
            let call = host_call(&[arg]);
            assert_eq!(sensitive(&call, &pin), warning, "{arg}");
            assert_eq!(state_target(&call, &pin), state, "{arg}");
        }
    }

    #[test]
    fn program_resolution_rejects_workspace_path_and_detects_changed_identity() {
        let fixture = fixture();
        let path = fixture._dir.path().to_str().unwrap();
        let pinned = pin("sh", &fixture.workspace, path).unwrap();
        assert_eq!(pinned.executable, fixture.program.executable);
        assert!(pinned.verify(&fixture.workspace).is_ok());
        for bad in [
            String::new(),
            ".".into(),
            format!("{path}:"),
            fixture.workspace.display().to_string(),
        ] {
            assert!(pin("sh", &fixture.workspace, &bad).is_err(), "{bad}");
        }
        let alias = fixture._dir.path().join("alias");
        std::os::unix::fs::symlink(&fixture.workspace, &alias).unwrap();
        assert!(pin("sh", &fixture.workspace, alias.to_str().unwrap()).is_err());
        let replacement = fixture._dir.path().join("replacement");
        std::fs::copy(&fixture.program.executable, &replacement).unwrap();
        std::fs::rename(replacement, &fixture.program.executable).unwrap();
        assert!(pinned.verify(&fixture.workspace).is_err());
    }

    #[test]
    fn absolute_programs_must_match_path_lookup_and_workspace_aliases_are_excluded() {
        let fixture = fixture();
        let root = fixture._dir.path().canonicalize().unwrap();
        let allowed = root.to_str().unwrap();
        assert!(pin(&fixture.program.executable, &fixture.workspace, allowed).is_ok());
        assert!(
            pin(
                &fixture.program.executable,
                &fixture.workspace,
                "/usr/bin:/bin"
            )
            .is_err()
        );
        let shadow = root.join("shadow");
        std::fs::create_dir(&shadow).unwrap();
        std::fs::copy(&fixture.program.executable, shadow.join("sh")).unwrap();
        assert!(
            pin(
                &fixture.program.executable,
                &fixture.workspace,
                &format!("{}:{allowed}", shadow.display())
            )
            .is_err()
        );
        let alias = root.join("workspace-alias");
        std::os::unix::fs::symlink(&fixture.workspace, &alias).unwrap();
        let pinned = pin("sh", &alias, allowed).unwrap();
        assert_eq!(pinned.workspace, fixture.workspace.to_str().unwrap());
        assert!(pinned.verify(&alias).is_ok());
        std::fs::copy(&fixture.program.executable, fixture.workspace.join("sh")).unwrap();
        for path in [&alias, &fixture.workspace] {
            assert!(pin("sh", &alias, path.to_str().unwrap()).is_err());
            assert!(pin(path.join("sh").to_str().unwrap(), &alias, allowed).is_err());
        }
    }

    #[test]
    fn privilege_aliases_and_handoff_preserve_exact_inert_arguments() {
        let fixture = fixture();
        for name in PRIVILEGE_TOOLS {
            assert!(prohibited(name, &[]));
            assert!(prohibited(&format!("/usr/bin/{name}"), &[]));
            let executable = fixture._dir.path().join(name);
            std::fs::copy(&fixture.program.executable, &executable).unwrap();
            let alias = fixture._dir.path().join(format!("alias-{name}"));
            std::os::unix::fs::symlink(&executable, &alias).unwrap();
            let resolved = pin(
                alias.to_str().unwrap(),
                &fixture.workspace,
                fixture._dir.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(prohibited(&resolved.executable, &[]));
        }
        for name in ["sudo-helper", "SUDO", "env", "sh"] {
            assert!(!prohibited(name, &[]));
        }
        let call = host_call(&["a b", "'", "\n", "$(secret)"]);
        let output = manual_handoff(&call);
        assert!(output.contains(r#"["find","a b","'","\n","$(secret)"]"#));
        assert!(output.contains("Please run this command by hand:"));
    }

    #[test]
    fn host_launcher() {
        let Ok(value) = std::env::var("ARANY_TEST_HOST_ARGS") else {
            return;
        };
        let (executable, args): (std::path::PathBuf, Vec<String>) =
            serde_json::from_str(&value).unwrap();
        assert_eq!(
            host_exec(executable.to_str().unwrap(), &args),
            std::process::ExitCode::SUCCESS
        );
    }

    struct Fixture {
        launcher: std::path::PathBuf,
        _dir: tempfile::TempDir,
        workspace: std::path::PathBuf,
        program: Program,
        identity: WorkspaceIdentity,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace = root.join("work");
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        let shell = root.join("sh");
        std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), &shell).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = Program {
            name: "sh".into(),
            executable: shell.to_str().unwrap().into(),
            sha256: hex_digest(&std::fs::read(&shell).unwrap()),
            interpreter: true,
            inputs: Vec::new(),
        };
        let identity = super::super::workspace_identity(&workspace).unwrap();
        Fixture {
            launcher: std::env::current_exe().unwrap(),
            _dir: dir,
            workspace,
            program,
            identity,
        }
    }

    impl Fixture {
        fn request<'a>(&'a self, args: &'a [String], cwd: &'a str, runtime_ms: u32) -> Request<'a> {
            Request {
                launcher: &self.launcher,
                program: &self.program,
                host: None,
                args,
                cwd,
                workspace: &self.workspace,
                workspace_identity: self.identity,
                runtime_grants: &[],
                result_bytes: 64 * 1024,
                runtime_ms,
            }
        }

        async fn sh(&self, script: &str, cwd: &str, runtime_ms: u32) -> Outcome {
            let args = ["-c".to_owned(), script.to_owned()];
            run(self.request(&args, cwd, runtime_ms), Cancellation::new()).await
        }
    }

    fn stdout(outcome: &Outcome) -> String {
        let result: super::super::types::CommandResult =
            serde_json::from_str(&outcome.output).unwrap();
        result
            .stdout
            .strip_prefix("\nrunning 1 test\n")
            .unwrap_or(&result.stdout)
            .to_owned()
    }

    #[tokio::test]
    async fn unguarded_run_uses_pinned_executable_explicit_argv_cleared_env_and_real_workspace_cwd()
    {
        let fixture = fixture();
        let outcome = fixture
            .sh("env; echo \"cwd: $(pwd -P)\"; touch made", "sub", 10_000)
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Succeeded);
        assert!(outcome.spawned);
        let text = stdout(&outcome);
        let names: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .filter(|name| name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .collect();
        let allowed = [
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "TERM",
            "NO_COLOR",
            "ARANY_TOOL_WORKSPACE",
            "ARANY_UNGUARDED",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
            #[cfg(target_os = "macos")]
            "__CF_USER_TEXT_ENCODING",
        ];
        assert!(names.iter().all(|name| allowed.contains(name)), "{names:?}");
        let home = directories::BaseDirs::new().unwrap();
        assert!(text.contains(&format!("HOME={}\n", home.home_dir().display())));
        assert!(text.contains("ARANY_UNGUARDED=1\n") && text.contains("NO_COLOR=1\n"));
        let real = fixture.workspace.join("sub");
        assert!(text.contains(&format!("cwd: {}\n", real.display())));
        assert!(text.contains(&format!(
            "ARANY_TOOL_WORKSPACE={}\n",
            fixture.workspace.display()
        )));
        assert!(real.join("made").is_file());
        assert!(outcome.descendants_stopped);
    }

    #[tokio::test]
    async fn unguarded_run_times_out_and_kills_its_process_group() {
        let fixture = fixture();
        let started = std::time::Instant::now();
        let outcome = fixture
            .sh(
                "printf partial; printf error >&2; sleep 30 & echo $! > pid; sleep 30",
                "",
                1_000,
            )
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Limit);
        assert!(outcome.descendants_stopped);
        assert_eq!(outcome.limit_cause, Some(LimitCause::Timeout));
        assert_eq!(stdout(&outcome), "partial");
        let result: super::super::types::CommandResult =
            serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(result.stderr, "error");
        assert!(result.changeset.files.is_empty());
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(fixture.workspace.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let background = Pid::from_raw(pid).unwrap();
        assert!(rustix::process::test_kill_process(background) == Err(Errno::SRCH));
    }

    #[tokio::test]
    async fn unguarded_run_cancellation_records_cancelled_and_kills_the_group() {
        let fixture = fixture();
        let cancellation = Cancellation::new();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let args = ["-c".to_owned(), "sleep 30 & sleep 30".to_owned()];
        let outcome = run(fixture.request(&args, "", 20_000), cancellation).await;
        assert_eq!(outcome.disposition, ToolDisposition::Cancelled);
        assert!(outcome.spawned && outcome.descendants_stopped);
    }

    #[tokio::test]
    async fn unguarded_run_truncates_output_and_preserves_exit_status() {
        let fixture = fixture();
        let outcome = fixture
            .sh(
                "printf HEAD; printf '%1048568s' ''; printf TAIL",
                "",
                10_000,
            )
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Succeeded);
        let result: super::super::types::CommandResult =
            serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(
            result.stdout_total.bytes,
            1048576 + "\nrunning 1 test\n".len() as u64
        );
        assert!(stdout(&outcome).starts_with("HEAD") && result.stdout.ends_with("TAIL"));
        assert!(result.stdout.contains(" lines omitted; "));
        assert!(result.valid());
        assert!(outcome.spawned && outcome.descendants_stopped);
    }

    #[tokio::test]
    async fn unguarded_run_refuses_changed_pinned_executable_before_spawn() {
        let mut fixture = fixture();
        fixture.program.sha256 = "0".repeat(64);
        let outcome = fixture.sh("touch made", "", 10_000).await;
        assert_eq!(outcome.disposition, ToolDisposition::Failed);
        assert!(!outcome.spawned);
        assert!(!fixture.workspace.join("made").exists());
    }

    #[tokio::test]
    async fn unguarded_run_refuses_cwd_symlink_escape() {
        let fixture = fixture();
        let outside = fixture.workspace.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.workspace.join("link")).unwrap();
        for cwd in ["link", "link/missing", "missing", "missing/child"] {
            let outcome = fixture.sh("touch made", cwd, 10_000).await;
            assert_eq!(outcome.disposition, ToolDisposition::Denied, "{cwd}");
            assert!(!outcome.spawned);
        }
        assert!(!outside.join("made").exists());
    }

    #[test]
    fn unconfirmed_reap_or_group_is_uncertain_for_every_ending() {
        for (reaped, stopped) in [(false, true), (true, false), (false, false)] {
            for ended in [
                Ended::Timeout(Window::default(), Window::default(), 4096),
                Ended::Cancelled,
                Ended::Failed(ToolError::Operation),
            ] {
                let outcome = settle(ended, reaped, stopped);
                assert_eq!(outcome.disposition, ToolDisposition::Uncertain);
                assert_eq!(outcome.output, UNCONFIRMED);
                assert_eq!(outcome.limit_cause, None);
            }
        }
        assert_eq!(
            settle(
                Ended::Timeout(Window::default(), Window::default(), 4096),
                true,
                true
            )
            .disposition,
            ToolDisposition::Limit
        );
    }
}

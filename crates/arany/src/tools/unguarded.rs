use super::ToolError;
use super::cancel::Cancellation;
use super::config::Program;
use super::fs::{identity, open_directory, open_file};
use super::guard::{reap_owned_child, runtime_path};
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

const UNCONFIRMED: &str =
    "Unguarded run outcome unconfirmed; processes may still run. Do not retry automatically.";
const REAP_BOUND: Duration = Duration::from_secs(1);
const GROUP_POLL: Duration = Duration::from_millis(10);
const GROUP_POLLS: u32 = 100;

pub(super) struct Request<'a> {
    pub(super) program: &'a Program,
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
    let (mut command, cap) = match prepare(&request) {
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
    let mut command = Command::new(executable);
    command
        .args(request.args)
        .current_dir(request.workspace.join(request.cwd))
        .env_clear()
        .env("PATH", runtime_path(request.runtime_grants)?)
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
        .process_group(0)
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

    struct Fixture {
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
            _dir: dir,
            workspace,
            program,
            identity,
        }
    }

    impl Fixture {
        fn request<'a>(&'a self, args: &'a [String], cwd: &'a str, runtime_ms: u32) -> Request<'a> {
            Request {
                program: &self.program,
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
        result.stdout
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
        assert_eq!(result.stdout_total.bytes, 1048576);
        assert!(result.stdout.starts_with("HEAD") && result.stdout.ends_with("TAIL"));
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

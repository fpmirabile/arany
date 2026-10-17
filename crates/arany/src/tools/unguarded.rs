use super::ToolError;
use super::cancel::Cancellation;
use super::config::Program;
use super::fs::{identity, open_absolute, open_directory, open_file, read_regular};
use super::guard::{read_capped, reap_owned_child, runtime_path};
use super::toolchains::RuntimeGrant;
use super::types::{CommandChangeset, CommandResult, ToolDisposition, hex_digest};
use crate::session::WorkspaceIdentity;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const MAX_EXECUTABLE_BYTES: usize = 256 * 1024 * 1024;
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
    pub(super) output: String,
    pub(super) spawned: bool,
    pub(super) descendants_stopped: bool,
}

impl Outcome {
    fn unstarted(disposition: ToolDisposition, output: &str) -> Self {
        Self {
            disposition,
            output: output.into(),
            spawned: false,
            descendants_stopped: false,
        }
    }

    fn started(disposition: ToolDisposition, output: &str, descendants_stopped: bool) -> Self {
        Self {
            disposition,
            output: output.into(),
            spawned: true,
            descendants_stopped,
        }
    }
}

enum Ended {
    Done(std::process::ExitStatus, Vec<u8>, Vec<u8>),
    Timeout,
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
    // The leader's pid stays reserved as the group id while any member lives, so this sweep cannot hit another group.
    let stopped = match group {
        Some(group) => stop_group(group).await,
        None => false,
    };
    if !reaped {
        return Outcome::started(
            ToolDisposition::Uncertain,
            "Unguarded run outcome unconfirmed; processes may still run. Do not retry automatically.",
            false,
        );
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
        Ended::Timeout | Ended::Failed(ToolError::Limit) => Outcome::started(
            ToolDisposition::Limit,
            "Tool result or resource limit reached",
            stopped,
        ),
        Ended::Failed(_) => Outcome::started(
            ToolDisposition::Failed,
            "Unguarded run failed; no automatic retry",
            stopped,
        ),
        Ended::Done(status, stdout, stderr) => {
            let result = CommandResult {
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                changeset: CommandChangeset::default(),
            };
            let output = serde_json::to_string(&result)
                .ok()
                .filter(|_| result.valid());
            let Some(output) = output else {
                return Outcome::started(
                    ToolDisposition::Limit,
                    "Tool result or resource limit reached",
                    stopped,
                );
            };
            let disposition = if result.exit_code == Some(0) {
                ToolDisposition::Succeeded
            } else {
                ToolDisposition::Failed
            };
            Outcome {
                disposition,
                output,
                spawned: true,
                descendants_stopped: stopped,
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
    let work = tokio::time::timeout(deadline, async {
        tokio::try_join!(
            async {
                reap_owned_child(child)
                    .await
                    .map_err(|_| ToolError::Operation)
            },
            read_capped(out, cap),
            read_capped(err, cap)
        )
    });
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Ended::Cancelled,
        joined = work => match joined {
            Ok(Ok((status, stdout, stderr))) => Ended::Done(status, stdout, stderr),
            Ok(Err(error)) => Ended::Failed(error),
            Err(_) => Ended::Timeout,
        },
    }
}

async fn stop_group(group: Pid) -> bool {
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
    let executable = Path::new(&request.program.executable);
    let bytes = read_regular(
        open_absolute(&request.program.executable)?,
        MAX_EXECUTABLE_BYTES,
    )?;
    if hex_digest(&bytes) != request.program.sha256 {
        return Err(ToolError::ChangedInput);
    }
    let workspace = open_directory(request.workspace)?;
    let found = identity(&workspace)?;
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
    if !open_file(&workspace, cwd)?
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
    Ok((
        command,
        (request.result_bytes as usize / 2).saturating_sub(128),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let result: CommandResult = serde_json::from_str(&outcome.output).unwrap();
        result.stdout
    }

    #[tokio::test]
    async fn unguarded_run_uses_pinned_executable_explicit_argv_cleared_env_and_real_workspace_cwd()
    {
        let fixture = fixture();
        let outcome = fixture
            .sh(
                "env; echo cwd=$(pwd -P); touch made; echo \"$1\"",
                "sub",
                10_000,
            )
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Succeeded);
        assert!(outcome.spawned);
        let text = stdout(&outcome);
        let names: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .filter(|name| name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
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
        assert!(text.contains(&format!("cwd={}\n", real.display())));
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
            .sh("sleep 30 & echo $! > pid; sleep 30", "", 1_000)
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Limit);
        assert!(outcome.descendants_stopped);
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
    async fn unguarded_run_caps_output_and_records_limit() {
        let fixture = fixture();
        let outcome = fixture
            .sh(
                "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
                "",
                10_000,
            )
            .await;
        assert_eq!(outcome.disposition, ToolDisposition::Limit);
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
        for cwd in ["link", "missing"] {
            let outcome = fixture.sh("touch made", cwd, 10_000).await;
            assert_eq!(outcome.disposition, ToolDisposition::Denied);
            assert!(!outcome.spawned);
        }
        assert!(!outside.join("made").exists());
    }
}

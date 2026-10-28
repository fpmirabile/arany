use super::*;
use crate::session::{
    EnvironmentCaptureIntent, EnvironmentCaptureStage as Stage, LoginShellIdentity,
};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) const LOGIN_SHELL_CAPTURE_TIMEOUT_MS: u64 = 10_000;
pub(crate) const LOGIN_SHELL_CAPTURE_OUTPUT_BYTES: usize = 64 * 1024;
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(LOGIN_SHELL_CAPTURE_TIMEOUT_MS);
const CAPTURE_CHUNK_BYTES: usize = 4096;
pub(crate) const CAPTURE_ARGS: [&str; 2] = ["-lc", "env -0"];
#[cfg(test)]
mod tests;

pub(crate) fn intent(
    environment: &CommandEnvironment,
    workspace: &Path,
) -> Result<EnvironmentCaptureIntent, ToolError> {
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .map_err(|_| ToolError::Configuration)?
        .ok_or(ToolError::Configuration)?;
    let shell = user.shell;
    if !shell.is_absolute()
        || !matches!(
            shell.file_name().and_then(|name| name.to_str()),
            Some("zsh" | "bash" | "sh")
        )
    {
        return Err(ToolError::Configuration);
    }
    let home = user.dir;
    if !home.is_absolute() {
        return Err(ToolError::Configuration);
    }
    prepare_intent(environment, &shell, &home, workspace)
}

fn prepare_intent(
    environment: &CommandEnvironment,
    shell: &Path,
    home: &Path,
    workspace: &Path,
) -> Result<EnvironmentCaptureIntent, ToolError> {
    let shell = shell.canonicalize().map_err(|_| ToolError::Configuration)?;
    let home = home.canonicalize().map_err(|_| ToolError::Configuration)?;
    let workspace = workspace.canonicalize().map_err(|_| ToolError::Path)?;
    if shell.starts_with(&workspace) || home.starts_with(&workspace) {
        return Err(ToolError::Configuration);
    }
    let file = super::super::fs::open_absolute(shell.to_str().ok_or(ToolError::Path)?)?;
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(ToolError::Configuration);
    }
    let bytes = super::super::fs::read_regular(file, super::super::types::MAX_SNAPSHOT_BYTES)?;
    if !super::super::executable::has_magic(
        &bytes,
        super::super::executable::EXECUTABLES.program_magics,
    ) {
        return Err(ToolError::Configuration);
    }
    let home_identity = identity(&open_directory(&home)?)?;
    Ok(EnvironmentCaptureIntent {
        operation_id: uuid::Uuid::now_v7(),
        shell: LoginShellIdentity {
            path: shell.to_str().ok_or(ToolError::Path)?.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
            sha256: crate::validate::hex_digest(&bytes),
        },
        home: home.to_str().ok_or(ToolError::Path)?.into(),
        home_device: home_identity.0,
        home_inode: home_identity.1,
        environment_sha256: environment.sha256().into(),
    })
}

fn command(
    launcher: &Path,
    intent: &EnvironmentCaptureIntent,
    environment: &CommandEnvironment,
) -> Result<Command, ToolError> {
    let shell = &intent.shell;
    let file = super::super::fs::open_absolute(&shell.path)?;
    let metadata = file.metadata().map_err(|_| ToolError::ChangedInput)?;
    let bytes = super::super::fs::read_regular(file, super::super::types::MAX_SNAPSHOT_BYTES)?;
    let home = identity(&open_directory(Path::new(&intent.home))?)?;
    if (metadata.dev(), metadata.ino()) != (shell.device, shell.inode)
        || crate::validate::hex_digest(&bytes) != shell.sha256
        || (home.0, home.1) != (intent.home_device, intent.home_inode)
        || environment.sha256() != intent.environment_sha256
    {
        return Err(ToolError::ChangedInput);
    }
    let mut command = Command::new(launcher);
    command.env_clear().envs(environment.names());
    command
        .env("PATH", environment.path_string())
        .env("HOME", &intent.home)
        .env("NO_COLOR", "1")
        .env("ARANY_UNGUARDED", "1");
    command
        .arg("--internal-host-command")
        .arg(&shell.path)
        .args(CAPTURE_ARGS);
    command
        .current_dir(&intent.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

pub(crate) async fn capture(
    launcher: PathBuf,
    intent: EnvironmentCaptureIntent,
    environment: CommandEnvironment,
) -> Result<CommandEnvironment, Stage> {
    let command = command(&launcher, &intent, &environment).map_err(|_| Stage::PinnedInput)?;
    run(command, intent, environment).await
}

async fn run(
    mut command: Command,
    intent: EnvironmentCaptureIntent,
    environment: CommandEnvironment,
) -> Result<CommandEnvironment, Stage> {
    let mut child = command.spawn().map_err(|_| Stage::Spawn)?;
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    let result = collect(&mut child).await;
    let reaped = if matches!(result, Ok((_, _))) {
        true
    } else {
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = child.start_kill();
        tokio::time::timeout(REAP_BOUND, reap_owned_child(&mut child))
            .await
            .is_ok_and(|status| status.is_ok())
    };
    let stopped = match group {
        Some(group) => stop_group(group).await,
        None => false,
    };
    if !reaped || !stopped {
        return Err(Stage::Uncertain);
    }
    let (status, bytes) = result?;
    if !status.success() {
        return Err(Stage::Exit);
    }
    environment
        .capture_output(intent.shell, &bytes)
        .map_err(|_| Stage::InvalidEnvironment)
}

async fn collect(
    child: &mut tokio::process::Child,
) -> Result<(std::process::ExitStatus, Vec<u8>), Stage> {
    let stdout = child.stdout.take().ok_or(Stage::Uncertain)?;
    let stderr = child.stderr.take().ok_or(Stage::Uncertain)?;
    let total = AtomicUsize::new(0);
    tokio::time::timeout(CAPTURE_TIMEOUT, async {
        let (status, stdout, _) = tokio::try_join!(
            async { reap_owned_child(child).await.map_err(|_| Stage::Uncertain) },
            read_pipe(stdout, true, &total),
            read_pipe(stderr, false, &total),
        )?;
        Ok((status, stdout))
    })
    .await
    .map_err(|_| Stage::Timeout)?
}

async fn read_pipe(
    mut reader: impl AsyncRead + Unpin,
    keep: bool,
    total: &AtomicUsize,
) -> Result<Vec<u8>, Stage> {
    let mut result = Vec::new();
    let mut buffer = [0; CAPTURE_CHUNK_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| Stage::Uncertain)?;
        if count == 0 {
            return Ok(result);
        }
        if total.fetch_add(count, Ordering::Relaxed) + count > LOGIN_SHELL_CAPTURE_OUTPUT_BYTES {
            return Err(Stage::OutputLimit);
        }
        if keep {
            result.extend_from_slice(&buffer[..count]);
        }
    }
}

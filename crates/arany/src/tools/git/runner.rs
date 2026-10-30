use super::super::ToolError;
use super::super::config::Program;
use super::super::types::MAX_SNAPSHOT_BYTES;
use crate::validate::hex_digest;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(super) const GIT_CAPTURE_BYTES: usize = 256 * 1024;
pub(super) const STDERR_BYTES: usize = 4 * 1024;
// The cage: forced on every invocation, after the user's configuration so it wins.
const CAGE: [&str; 3] = [
    "core.fsmonitor=false",
    "core.pager=cat",
    "core.editor=false",
];
const NO_HOOKS: &str = "/dev/null";
const NO_NETWORK: &str = "protocol.allow=never";
// The user's own settings a Git or gh process keeps, when set: configuration home, agent and gh profile.
const USER_VARIABLES: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "SSH_AUTH_SOCK",
    "GH_CONFIG_DIR",
    "GH_HOST",
];
const CONFIG_LIST_BYTES: usize = 64 * 1024;
const CONFIG_ADMISSION_TIME: Duration = Duration::from_secs(2);
const MAX_PATH_DIRECTORIES: usize = 64;
const MAX_LOCK_SCAN_ENTRIES: usize = 4096;
const REAP_GRACE: Duration = Duration::from_secs(1);
const CONFIG_UNREADABLE: &str =
    "Git could not list its configuration, so Arany did not run Git; check `git config --list`.";

// Configuration values Git runs as programs; one naming a file inside the folder refuses.
fn executable_key(key: &str, value: &str) -> bool {
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (subsection, name) = match rest.rsplit_once('.') {
        Some((_, name)) => (true, name),
        None => (false, rest),
    };
    match (section, subsection, name) {
        ("filter", true, "clean" | "smudge" | "process")
        | ("diff", true, "command" | "textconv")
        | ("diff", false, "external")
        | ("merge", true, "driver")
        | ("core", false, "sshcommand" | "askpass" | "gitproxy")
        | ("credential", _, "helper")
        | ("gpg", _, "program")
        | ("sequence", false, "editor")
        | ("pager", false, _)
        | ("remote", true, "uploadpack" | "receivepack") => true,
        ("alias", false, _) => value.starts_with('!'),
        _ => false,
    }
}

// The user's environment Git runs with, captured at Run admission; model-writable roots never reach PATH.
#[derive(Clone, Debug)]
pub(crate) struct Environment {
    home: PathBuf,
    path: OsString,
    variables: Vec<(&'static str, OsString)>,
    workspace: PathBuf,
    editors: PathBuf,
}

impl Environment {
    pub(crate) fn capture(workspace: &Path, editors: &Path) -> Option<Self> {
        Self::from_variables(workspace, editors, |name| std::env::var_os(name))
    }

    pub(crate) fn from_variables(
        workspace: &Path,
        editors: &Path,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Option<Self> {
        let home = variable("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())?;
        let mut environment = Self {
            home,
            path: OsString::new(),
            variables: USER_VARIABLES
                .into_iter()
                .filter_map(|name| variable(name).map(|value| (name, value)))
                .collect(),
            workspace: std::fs::canonicalize(workspace).ok()?,
            editors: canonical(editors),
        };
        let directories: Vec<PathBuf> = std::env::split_paths(&variable("PATH")?)
            .filter(|directory| directory.is_absolute())
            .take(MAX_PATH_DIRECTORIES)
            .filter_map(|directory| std::fs::canonicalize(directory).ok())
            .filter(|directory| !environment.writable(directory))
            .collect();
        environment.path = std::env::join_paths(directories).ok()?;
        Some(environment)
    }

    // The Workspace outside its `.git`, and every editor workspace, are model-writable.
    fn writable(&self, path: &Path) -> bool {
        path.starts_with(&self.editors)
            || path.starts_with(&self.workspace) && !path.starts_with(self.workspace.join(".git"))
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(super) fn home(&self) -> &Path {
        &self.home
    }

    // A host process with the user's home, configuration and agent that never prompts.
    pub(super) fn command(&self, executable: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(executable);
        command
            .env_clear()
            .envs(self.variables.iter().map(|(name, value)| (name, value)))
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .process_group(0)
            .kill_on_drop(true);
        command
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let mut normal = PathBuf::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    normal.pop();
                }
                Component::CurDir => {}
                component => normal.push(component),
            }
        }
        normal
    })
}

fn executable_bytes(executable: &str) -> Result<Vec<u8>, ToolError> {
    use std::os::unix::fs::MetadataExt;
    let file = super::super::fs::open_absolute(executable)?;
    let metadata = file.metadata().map_err(|_| ToolError::Operation)?;
    if !metadata.is_file()
        || metadata.mode() & 0o6022 != 0
        || metadata.mode() & 0o111 == 0
        || metadata.len() > MAX_SNAPSHOT_BYTES as u64
    {
        return Err(ToolError::ChangedInput);
    }
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::Operation)?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

// The macOS `/usr/bin/git` shim opens an installer dialog when its developer directory lacks Git.
#[cfg(target_os = "macos")]
fn usable(executable: &Path) -> bool {
    executable != Path::new("/usr/bin/git")
        || std::fs::read_link("/var/db/xcode_select_link")
            .unwrap_or_else(|_| "/Library/Developer/CommandLineTools".into())
            .join("usr/bin/git")
            .is_file()
}

#[cfg(not(target_os = "macos"))]
fn usable(_: &Path) -> bool {
    true
}

// The first `git` on the filtered PATH, pinned by path and SHA-256 for one Run.
pub(crate) fn resolve(environment: &Environment) -> Option<Program> {
    std::env::split_paths(&environment.path).find_map(|directory| {
        let executable = std::fs::canonicalize(directory.join("git")).ok()?;
        if environment.writable(&executable) || !usable(&executable) {
            return None;
        }
        let executable = executable.to_str()?.to_owned();
        let sha256 = hex_digest(&executable_bytes(&executable).ok()?);
        Some(Program {
            name: "git".into(),
            executable,
            sha256,
            interpreter: false,
            inputs: Vec::new(),
        })
    })
}

// A Git that changed during the Run never runs.
pub(super) fn verify(program: &Program) -> Result<(), ToolError> {
    if hex_digest(&executable_bytes(&program.executable)?) != program.sha256 {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

pub(super) struct Captured {
    pub(super) status: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) overflow: bool,
}

// Optional stdin and the stdout capture bound.
pub(super) struct Io<'a> {
    pub(super) stdin: Option<&'a [u8]>,
    pub(super) stdout_bytes: usize,
}

impl Default for Io<'_> {
    fn default() -> Self {
        Self {
            stdin: None,
            stdout_bytes: GIT_CAPTURE_BYTES,
        }
    }
}

// Drain excess stderr to avoid killing a verbose writer; cut stdout off to stop overlong output.
async fn read_prefix(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    cap: usize,
    drain: bool,
) -> Result<(Vec<u8>, bool), ToolError> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ToolError::Operation)?;
    let overflow = bytes.len() > cap;
    bytes.truncate(cap);
    if overflow && drain {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    }
    Ok((bytes, overflow))
}

// One repository and work tree, run with the user's configuration under the cage.
pub(super) struct Git<'a> {
    pub(super) program: &'a Program,
    pub(super) environment: &'a Environment,
    pub(super) git_dir: PathBuf,
    pub(super) work_tree: PathBuf,
    // The accepted hooks directory; without it every invocation runs with hooks off.
    pub(super) hooks: Option<PathBuf>,
    exit: std::sync::Mutex<Option<i32>>,
}

impl<'a> Git<'a> {
    pub(super) fn workspace(program: &'a Program, environment: &'a Environment) -> Self {
        Self {
            program,
            environment,
            git_dir: environment.workspace.join(".git"),
            work_tree: environment.workspace.clone(),
            hooks: None,
            exit: Default::default(),
        }
    }

    // The exit status of the last completed invocation.
    pub(super) fn exit_code(&self) -> Option<i32> {
        self.exit.lock().ok().and_then(|exit| *exit)
    }

    // Only a push reaches the network; every other invocation refuses transports and lazy fetches.
    pub(super) fn command(&self, argv: &[String], network: bool) -> tokio::process::Command {
        let mut command = self
            .environment
            .command(Path::new(&self.program.executable));
        let hooks = self.hooks.as_deref().unwrap_or(Path::new(NO_HOOKS));
        let mut setting = OsString::from("core.hooksPath=");
        setting.push(hooks);
        command.arg("-c").arg(setting);
        for setting in CAGE {
            command.args(["-c", setting]);
        }
        if !network {
            command
                .args(["-c", NO_NETWORK])
                .env("GIT_NO_LAZY_FETCH", "1");
        }
        command
            .args(argv)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .current_dir(&self.work_tree);
        command
    }

    pub(super) async fn run(
        &self,
        argv: &[String],
        io: Io<'_>,
        deadline: tokio::time::Instant,
    ) -> Result<Captured, ToolError> {
        verify(self.program)?;
        let mut command = self.command(argv, false);
        command
            .stdin(if io.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = super::super::guard::spawn_before(&mut command, deadline)?;
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or(ToolError::Operation)?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().ok_or(ToolError::Operation)?;
        let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
        let result = tokio::time::timeout_at(deadline, async {
            // Feed stdin while draining stdout, or a full pipe deadlocks both sides.
            let feed = async {
                if let (Some(mut input), Some(bytes)) = (input, io.stdin) {
                    tokio::io::AsyncWriteExt::write_all(&mut input, bytes)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                }
                Ok::<_, ToolError>(())
            };
            let ((stdout, overflow), (stderr, _), ()) = tokio::try_join!(
                read_prefix(stdout, io.stdout_bytes, false),
                read_prefix(stderr, STDERR_BYTES, true),
                feed
            )?;
            let status = super::super::guard::reap_owned_child(&mut child)
                .await
                .map_err(|_| ToolError::Operation)?;
            Ok::<_, ToolError>(Captured {
                status: status.code(),
                stdout,
                stderr,
                overflow,
            })
        })
        .await;
        if result.is_err() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            let reaped = matches!(
                tokio::time::timeout(
                    REAP_GRACE,
                    super::super::guard::reap_owned_child(&mut child)
                )
                .await,
                Ok(Ok(_))
            );
            let stopped = super::super::unguarded::stop_group(group).await;
            return Err(if reaped && stopped {
                ToolError::Timeout(String::new())
            } else {
                ToolError::Operation
            });
        }
        let stopped = super::super::unguarded::stop_group(group).await;
        match result {
            Ok(Ok(captured)) if stopped => {
                if let Ok(mut exit) = self.exit.lock() {
                    *exit = captured.status;
                }
                Ok(captured)
            }
            Ok(Ok(_)) | Err(_) => Err(ToolError::Operation),
            Ok(Err(error)) => Err(error),
        }
    }

    // Refuses, without running the operation, configuration that would run or include a folder file.
    pub(super) async fn admit(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<String>, ToolError> {
        let deadline = deadline.min(tokio::time::Instant::now() + CONFIG_ADMISSION_TIME);
        let argv = ["config", "--list", "--show-origin", "-z"].map(String::from);
        let captured = self
            .run(
                &argv,
                Io {
                    stdin: None,
                    stdout_bytes: CONFIG_LIST_BYTES,
                },
                deadline,
            )
            .await?;
        if captured.overflow || captured.status != Some(0) {
            return Ok(Some(CONFIG_UNREADABLE.into()));
        }
        Ok(self.refusal(&captured.stdout))
    }

    fn refusal(&self, listing: &[u8]) -> Option<String> {
        let mut fields = listing.split(|byte| *byte == 0);
        while let (Some(origin), Some(entry)) = (fields.next(), fields.next()) {
            let origin = String::from_utf8_lossy(origin);
            let entry = String::from_utf8_lossy(entry);
            let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
            if let Some(file) = origin.strip_prefix("file:")
                && self
                    .environment
                    .writable(&canonical(&self.work_tree.join(file)))
            {
                return Some(format!(
                    "Git would read configuration from {file}, inside this folder; Arany did not run Git."
                ));
            }
            if executable_key(key, value) && self.names_writable(value) {
                return Some(format!(
                    "Git configuration {key} would run a program inside this folder; Arany did not run Git."
                ));
            }
        }
        None
    }

    // A configured `insteadOf` or `pushInsteadOf` would send an approved push somewhere else.
    pub(super) async fn rewrites(
        &self,
        url: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool, ToolError> {
        let argv = ["config", "--get-regexp", r"^url\..*\.(push)?insteadof$"].map(String::from);
        let captured = self.run(&argv, Io::default(), deadline).await?;
        if captured.overflow || !matches!(captured.status, Some(0 | 1)) {
            return Err(ToolError::Configuration);
        }
        Ok(String::from_utf8_lossy(&captured.stdout)
            .lines()
            .filter_map(|line| line.split_once(' '))
            .any(|(_, prefix)| !prefix.is_empty() && url.starts_with(prefix)))
    }

    // ponytail: shell words are split on separators only; `$VAR` paths and bare programs reading folder files are not caught.
    fn names_writable(&self, value: &str) -> bool {
        value
            .trim_start_matches('!')
            .split(|character: char| {
                character.is_whitespace() || "\"'`;&|()=<>".contains(character)
            })
            .filter(|word| word.contains('/'))
            .any(|word| {
                let path = match word.strip_prefix("~/") {
                    Some(rest) => self.environment.home.join(rest),
                    None => self.work_tree.join(word),
                };
                self.environment.writable(&canonical(&path))
            })
    }
}

// Lock files an interrupted Git left behind, for the user to remove; never deleted here.
pub(crate) fn stale_locks(workspace: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut budget = MAX_LOCK_SCAN_ENTRIES;
    let root = workspace.join(".git");
    scan_locks(&root, ".git", &mut found, &mut budget);
    found
}

fn scan_locks(directory: &Path, shown: &str, found: &mut Vec<String>, budget: &mut usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = format!("{shown}/{name}");
        if kind.is_file() && name.ends_with(".lock") {
            found.push(path);
        } else if kind.is_dir() && (shown != ".git" || name == "refs") {
            scan_locks(&entry.path(), &path, found, budget);
        }
    }
}

#[cfg(test)]
mod tests;

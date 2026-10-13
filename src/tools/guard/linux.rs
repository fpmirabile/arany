use super::{
    CAPTURE_BYTES, GuardStage, RUNTIME_ROOT, Task, bootstrap_failure, capture, drain_stderr,
    encode_task, parse_ready, profile, read_capped, read_line_capped, read_observation,
};
use crate::engine::RunCancellation;
use crate::tools::types::{
    EffectIntent, MAX_CHANGESET_BYTES, MAX_CHANGESET_FILES, MAX_PROCESSES_OR_THREADS, MEMORY_BYTES,
    ToolCall, ToolObservation, digest,
};
use crate::tools::{ToolError, ToolRuntime};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

const MANAGER_DEADLINE: Duration = Duration::from_secs(4);
const MOUNTINFO_BYTES_CAP: usize = 1024 * 1024;
const MOUNTINFO_ENTRIES_CAP: usize = 4096;
const MOUNTINFO_LINE_CAP: usize = 16 * 1024;

pub(super) fn check_available() -> Result<(), ToolError> {
    profile::programs()?;
    use std::os::unix::fs::MetadataExt;
    for path in [
        "/usr/bin/bwrap",
        "/usr/bin/systemd-run",
        "/usr/bin/systemctl",
        "/usr/bin/env",
        "/usr/bin/unshare",
    ] {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| ToolError::ProtectionUnavailable)?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o6022 != 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    let mut bytes = b"arany-guard-v10:shared-fifo-capacity;owned-host-seed;retained-reservation;no-buffered-go-before-ready;pinned-skill-digest-pre-go;fixed-root-owned-unshare-bootstrap-user-map-current-recursive-private;private-mount,user,pid,net,ipc,uts;no-userns;no-new-privs;zero-caps;not-dumpable;cpu25000/100000;memory536870912;swap0;oom-group1;pids64;scratch67108864;workspace67108864;nofile64;core0;no-inherited-host-handles;typed-native-or-fresh-snapshot;namespace-descendant-stop-kill-positive-absence-before-command-changeset;strict-bounded-no-follow-changeset;owned-cgroup-cleanup;runtime-root-/usr-excludes-workspace-state-account;versioned-exact-ro-runtime-cache-mounts;bounded-namespace-mountinfo-private-root-and-unique-exact-private-aliases-no-descendants-before-ready-and-after-go".to_vec();
    bytes.extend_from_slice(&profile::profile_digest()?);
    bytes.extend_from_slice(&(MAX_CHANGESET_FILES as u64).to_le_bytes());
    bytes.extend_from_slice(&(MAX_CHANGESET_BYTES as u64).to_le_bytes());
    Ok(digest(&bytes))
}

pub(super) async fn stop_command_descendants(
    parent: &profile::Namespaces,
) -> Result<(), ToolError> {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group};

    let own = std::process::id();
    if !matches!(own, 1 | 2) || !profile::Namespaces::read()?.isolated_from(parent) {
        return Err(ToolError::ProtectionUnavailable);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        // Pid::INIT selects kill(-1), excluding this helper and the trusted namespace init.
        for signal in [Signal::STOP, Signal::KILL] {
            match kill_process_group(Pid::INIT, signal) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(_) => return Err(ToolError::ProtectionUnavailable),
            }
        }
        let mut count = 0usize;
        let mut live = false;
        for (index, entry) in std::fs::read_dir("/proc")
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .enumerate()
        {
            if index >= 512 {
                return Err(ToolError::Limit);
            }
            let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(ToolError::ProtectionUnavailable)?;
            if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let pid = name
                .parse::<u32>()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            count += 1;
            if count > MAX_PROCESSES_OR_THREADS as usize + 2 {
                return Err(ToolError::Limit);
            }
            live |= pid != 1 && pid != own;
        }
        if !live {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ToolError::ProtectionUnavailable);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drops capabilities, installs the syscall profile and verifies the result before Ready.
pub(super) fn harden() -> Result<(), ToolError> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    use rustix::process::{Resource, Rlimit, setrlimit};
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(64),
            maximum: Some(64),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    profile::install()?;
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then_some(value.trim())
        })
    };
    if field("NoNewPrivs") != Some("1")
        || field("CapEff") != Some("0000000000000000")
        || field("CapPrm") != Some("0000000000000000")
        || field("CapInh") != Some("0000000000000000")
        || field("CapAmb") != Some("0000000000000000")
        || field("Seccomp") != Some("2")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn manager_command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    let runtime = format!("/run/user/{}", rustix::process::geteuid().as_raw());
    command.env("XDG_RUNTIME_DIR", &runtime).env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={runtime}/bus"),
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

struct Unit {
    name: String,
    invocation: Option<String>,
    group: Option<cap_std::fs::Dir>,
    kill: Option<cap_std::fs::File>,
}

impl Unit {
    async fn properties(&self) -> Result<serde_json::Value, ToolError> {
        let mut command = manager_command("/usr/bin/systemctl");
        command.args(["--user", "--no-ask-password", "show", &self.name,
            "--property=Id,LoadState,InvocationID,ControlGroup,MainPID,ActiveState,SubState,ExecMainCode,ExecMainStatus"]);
        let (status, stdout, _) = capture(
            command
                .spawn()
                .map_err(|_| ToolError::ProtectionUnavailable)?,
            4096,
            MANAGER_DEADLINE,
        )
        .await?;
        if !status.success() {
            return Err(ToolError::ProtectionUnavailable);
        }
        let text = std::str::from_utf8(&stdout).map_err(|_| ToolError::ProtectionUnavailable)?;
        let mut values = serde_json::Map::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or(ToolError::ProtectionUnavailable)?;
            if values
                .insert(key.to_owned(), serde_json::Value::String(value.to_owned()))
                .is_some()
            {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        Ok(serde_json::Value::Object(values))
    }

    async fn admit(&mut self) -> Result<(), ToolError> {
        let values = self.properties().await?;
        let group = values["ControlGroup"]
            .as_str()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let invocation = values["InvocationID"]
            .as_str()
            .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(ToolError::ProtectionUnavailable)?;
        if values["Id"].as_str() != Some(&self.name)
            || values["ActiveState"] != "active"
            || !group.ends_with(&self.name)
            || !group.starts_with("/user.slice/")
            || group.contains("..")
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let dir = crate::tools::fs::open_directory(
            &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
        )?;
        self.invocation = Some(invocation.to_owned());
        self.group = Some(
            dir.try_clone()
                .map_err(|_| ToolError::ProtectionUnavailable)?,
        );
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true);
        self.kill = Some(
            dir.open_with("cgroup.kill", &options)
                .map_err(|_| ToolError::ProtectionUnavailable)?,
        );
        for (name, expected) in [
            ("memory.max", MEMORY_BYTES.to_string()),
            ("memory.swap.max", "0".into()),
            ("memory.oom.group", "1".into()),
            ("pids.max", MAX_PROCESSES_OR_THREADS.to_string()),
            ("cpu.max", "25000 100000".into()),
        ] {
            let mut file = dir
                .open(name)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            let mut actual = String::new();
            Read::by_ref(&mut file)
                .take(256)
                .read_to_string(&mut actual)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if actual.trim() != expected {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        let main_pid = values["MainPID"]
            .as_str()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or(ToolError::ProtectionUnavailable)?;
        let mut members = String::new();
        dir.open("cgroup.procs")
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .take(4096)
            .read_to_string(&mut members)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if members.lines().count() > MAX_PROCESSES_OR_THREADS as usize
            || !members
                .lines()
                .any(|line| line.parse::<u32>() == Ok(main_pid))
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), ToolError> {
        let values = match self.properties().await {
            Ok(values) => values,
            Err(_) if self.empty()? => return Ok(()),
            Err(error) => return Err(error),
        };
        if values["ActiveState"] == "inactive" && self.empty()? {
            return Ok(());
        }
        if !matches!(values["ActiveState"].as_str(), Some("inactive" | "failed"))
            && self
                .invocation
                .as_ref()
                .is_some_and(|id| values["InvocationID"].as_str() != Some(id))
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut command = manager_command("/usr/bin/systemctl");
        command.args(["--user", "--no-ask-password", "stop", &self.name]);
        let (status, _, _) = capture(
            command.spawn().map_err(|_| ToolError::Operation)?,
            4096,
            MANAGER_DEADLINE,
        )
        .await?;
        if !status.success() {
            if self.empty()? {
                return Ok(());
            }
            return Err(ToolError::Operation);
        }
        if self.group.is_some() && !self.empty()? {
            return Err(ToolError::Operation);
        }
        let values = match self.properties().await {
            Ok(values) => values,
            Err(_) if self.empty()? => return Ok(()),
            Err(error) => return Err(error),
        };
        if !matches!(values["ActiveState"].as_str(), Some("inactive" | "failed")) {
            return Err(ToolError::Operation);
        }
        if values["ActiveState"] == "failed" {
            let mut command = manager_command("/usr/bin/systemctl");
            command.args(["--user", "--no-ask-password", "reset-failed", &self.name]);
            let (status, _, _) = capture(
                command.spawn().map_err(|_| ToolError::Operation)?,
                4096,
                MANAGER_DEADLINE,
            )
            .await?;
            if !status.success() {
                return Err(ToolError::Operation);
            }
        }
        Ok(())
    }

    fn empty(&self) -> Result<bool, ToolError> {
        if let Some(group) = &self.group {
            let mut actual = String::new();
            match group.open("cgroup.events") {
                Ok(mut file) => {
                    Read::by_ref(&mut file)
                        .take(1024)
                        .read_to_string(&mut actual)
                        .map_err(|_| ToolError::Operation)?;
                    if !actual.lines().any(|line| line == "populated 0") {
                        return Ok(false);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(ToolError::Operation),
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn kill_group(&self) {
        if let Some(kill) = &self.kill {
            let mut file = kill;
            let _ = file.write_all(b"1");
            return;
        }
        if let Some(group) = &self.group {
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true);
            if let Ok(mut file) = group.open_with("cgroup.kill", &options) {
                let _ = file.write_all(b"1");
            }
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.kill_group();
    }
}

pub(super) async fn recover_reservation(id: uuid::Uuid) -> Result<bool, ToolError> {
    let mut unit = Unit {
        name: format!("arany-tool-{id}.service"),
        invocation: None,
        group: None,
        kill: None,
    };
    let values = unit.properties().await?;
    // No loaded unit cannot settle a controller interrupted during its launch request.
    if values["Id"].as_str() != Some(&unit.name) || values["LoadState"] != "loaded" {
        return Ok(false);
    }
    if matches!(values["ActiveState"].as_str(), Some("inactive" | "failed"))
        && values["MainPID"] == "0"
        && values["ControlGroup"] == ""
    {
        unit.stop().await?;
        return Ok(true);
    }
    let group = values["ControlGroup"]
        .as_str()
        .ok_or(ToolError::ProtectionUnavailable)?;
    if !group.starts_with("/user.slice/") || !group.ends_with(&unit.name) || group.contains("..") {
        return Err(ToolError::ProtectionUnavailable);
    }
    unit.group = Some(crate::tools::fs::open_directory(
        &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
    )?);
    unit.invocation = values["InvocationID"].as_str().map(str::to_owned);
    unit.stop().await?;
    unit.empty()
}

#[cfg(test)]
pub(super) fn stop_seed_fixture(id: uuid::Uuid) {
    use crate::test_process::BoundedOutput;
    let checked = std::panic::catch_unwind(|| {
        let name = format!("arany-tool-{id}.service");
        let mut stop = manager_command("/usr/bin/systemctl");
        stop.args(["--user", "--no-ask-password", "stop", &name]);
        let _ = stop
            .into_std()
            .bounded_output_for(MANAGER_DEADLINE, 4096)
            .unwrap();
        let mut show = manager_command("/usr/bin/systemctl");
        show.args([
            "--user",
            "show",
            &name,
            "--property=ActiveState,MainPID,ControlGroup",
        ]);
        let output = show
            .into_std()
            .bounded_output_for(MANAGER_DEADLINE, 4096)
            .unwrap();
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(
            text.lines()
                .any(|line| line == "ActiveState=inactive" || line == "ActiveState=failed")
        );
        assert!(text.lines().any(|line| line == "MainPID=0"));
        assert!(text.lines().any(|line| line == "ControlGroup="));
    });
    if let Err(error) = checked {
        if std::thread::panicking() {
            eprintln!("native seed fixture cleanup could not be verified: {id}");
        } else {
            std::panic::resume_unwind(error);
        }
    }
}

#[cfg(test)]
pub(super) async fn seed_unit_group(id: uuid::Uuid) -> cap_std::fs::Dir {
    let unit = Unit {
        name: format!("arany-tool-{id}.service"),
        invocation: None,
        group: None,
        kill: None,
    };
    let values = unit.properties().await.unwrap();
    assert_eq!(values["ActiveState"], "active");
    let group = values["ControlGroup"].as_str().unwrap();
    assert!(group.starts_with("/user.slice/") && group.ends_with(&unit.name));
    let dir = crate::tools::fs::open_directory(
        &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
    )
    .unwrap();
    assert!(
        dir.read_to_string("cgroup.events")
            .unwrap()
            .lines()
            .any(|line| line == "populated 1")
    );
    dir
}

#[cfg(test)]
pub(super) async fn hold_seed_unit(permit: &super::admission::Permit, seed: &Path) {
    let name = format!("arany-tool-{}.service", permit.id);
    let mut command = manager_command("/usr/bin/systemd-run");
    command
        .args([
            "--user",
            "--no-ask-password",
            "--quiet",
            "--wait",
            "--pipe",
            "--service-type=exec",
            "--unit",
            &name,
            "--property=Restart=no",
            "--property=KillMode=control-group",
            "--property=TimeoutStopSec=2s",
            "--property=RuntimeMaxSec=60s",
            "/usr/bin/unshare",
            "--user",
            "--map-current-user",
            "--mount",
            "--propagation",
            "private",
            "--keep-caps",
            "--",
            "/usr/bin/bwrap",
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--ro-bind",
        ])
        .arg(seed)
        .args([
            "/seed",
            "/usr/bin/sh",
            "-c",
            "test -r /seed/workspace/sample && printf 'READY\\n' && exec /usr/bin/sleep 600",
        ]);
    permit.launching().unwrap();
    let mut child = command.spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let ready = tokio::time::timeout(Duration::from_secs(10), read_line_capped(&mut stdout, 64))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, b"READY\n");
    permit.seed_checkpoint("native");
    panic!("native seed controller must be killed by its parent");
}

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ToolError::Operation),
        permit = super::admission::acquire(intent.id, intent.expires_at_ms, |queued| runtime.queue_changed(intent.id, queued)) => permit?,
    };
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        + Duration::from_secs(5);
    crate::tools::fs::validate_workspace(runtime)?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    let (seed_path, seed_dir) =
        permit.prepare_seed(&workspace, &runtime.config, intent.call.is_process())?;
    let runtime_grants = if intent.call.is_process() {
        runtime.config.runtime_grants.as_slice()
    } else {
        &[]
    };
    let runtime_path = runtime_path(runtime_grants)?;
    let mut unit = Unit {
        name: format!("arany-tool-{}.service", permit.id),
        invocation: None,
        group: None,
        kill: None,
    };
    let mut command = manager_command("/usr/bin/systemd-run");
    command.args([
        "--user",
        "--no-ask-password",
        "--quiet",
        "--description=Arany bounded Tool worker",
        "--pipe",
        "--wait",
        "--expand-environment=no",
        "--service-type=exec",
        "--unit",
        &unit.name,
        "--property=Restart=no",
        "--property=KillMode=control-group",
        "--property=SendSIGKILL=yes",
        "--property=FinalKillSignal=SIGKILL",
        "--property=TimeoutStartSec=5s",
        "--property=TimeoutStopSec=2s",
        "--property=RuntimeRandomizedExtraSec=0",
        "--property=RemainAfterExit=no",
        "--property=MemoryMax=536870912",
        "--property=MemorySwapMax=0",
        "--property=OOMPolicy=kill",
        "--property=TasksMax=64",
        "--property=CPUQuota=25%",
        "--property=CPUQuotaPeriodSec=100ms",
    ]);
    command
        .arg(format!(
            "--property=RuntimeMaxSec={}ms",
            intent.limits.runtime_ms
        ))
        .args([
            "/usr/bin/env",
            "-i",
            "PATH=/usr/bin:/bin",
            "/usr/bin/unshare",
            "--user",
            "--map-current-user",
            "--mount",
            "--propagation",
            "private",
            "--keep-caps",
            "--",
            "/usr/bin/bwrap",
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--new-session",
            "--die-with-parent",
            "--cap-drop",
            "ALL",
            "--clearenv",
            "--setenv",
            "PATH",
            &runtime_path,
            "--setenv",
            "HOME",
            "/scratch",
            "--setenv",
            "TMPDIR",
            "/scratch",
            "--setenv",
            "CARGO_TARGET_DIR",
            "/scratch/target",
            "--ro-bind",
            RUNTIME_ROOT,
            RUNTIME_ROOT,
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
            "67108864",
            "--tmpfs",
            "/scratch",
            "--ro-bind",
        ]);
    command.arg(&seed_path).arg("/seed");
    let mut runtime_directories = std::collections::BTreeSet::new();
    let mut mounts = Vec::new();
    for grant in runtime_grants {
        grant.verify_host()?;
        let target = grant.mounted_path()?;
        let root = if grant.kind.is_toolchain() {
            Path::new("/runtime")
        } else {
            Path::new("/scratch")
        };
        for parent in target.ancestors().take_while(|path| *path != root) {
            runtime_directories.insert(parent.to_owned());
        }
        mounts.push((grant.path.clone(), target));
    }
    let mut runtime_directories = runtime_directories.into_iter().collect::<Vec<_>>();
    runtime_directories.sort_by_key(|path| (path.components().count(), path.clone()));
    for directory in runtime_directories {
        command.args(["--dir"]).arg(directory);
    }
    for (source, target) in mounts {
        command.args(["--ro-bind"]).arg(source).arg(target);
    }
    for folder in ["programs", "inputs", "skills"] {
        if seed_dir.symlink_metadata(folder).is_ok() {
            command
                .args(["--ro-bind"])
                .arg(seed_path.join(folder))
                .arg(format!("/{folder}"));
        }
    }
    let native = matches!(
        intent.call,
        ToolCall::List { .. }
            | ToolCall::Read { .. }
            | ToolCall::Search { .. }
            | ToolCall::Write { .. }
            | ToolCall::Edit { .. }
            | ToolCall::Mkdir { .. }
            | ToolCall::ApplyChangeset { .. }
    );
    if native {
        command
            .arg(if intent.call.mutates() {
                "--bind"
            } else {
                "--ro-bind"
            })
            .arg(&runtime.workspace)
            .arg("/workspace");
    } else {
        command.args(["--size", "67108864", "--tmpfs", "/workspace"]);
    }
    command
        .arg("--ro-bind")
        .arg(&runtime.guard_executable)
        .arg("/arany")
        .args(["--chdir", "/", "/arany", "--internal-tool-guard"]);
    command.stdin(Stdio::piped());
    let parent_namespaces = profile::Namespaces::read()?;
    permit.launching()?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            permit.terminated()?;
            permit.cleanup().await?;
            return Err(ToolError::ProtectionUnavailable);
        }
    };
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, CAPTURE_BYTES));
    let execution = async {
        let task = encode_task(&Task {
            config: runtime.config.clone(),
            intent: intent.clone(),
            parent_namespaces: parent_namespaces.clone(),
        })?;
        stdin
            .write_all(&task)
            .await
            .map_err(|_| ToolError::Operation)?;
        let ready = parse_ready(&read_line_capped(&mut stdout, 2048).await?, intent)?;
        if ready.pid != 1 && ready.pid != 2 {
            return Err(ToolError::ProtectionUnavailable);
        }
        if !ready.namespaces.isolated_from(&parent_namespaces) {
            return Err(ToolError::ProtectionUnavailable);
        }
        unit.admit()
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?;
        *dispatched = true;
        stdin
            .write_all(b"GO\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        stdin.shutdown().await.map_err(|_| ToolError::Operation)?;
        let observation = read_observation(&mut stdout, intent).await?;
        if !child
            .wait()
            .await
            .map_err(|_| ToolError::Operation)?
            .success()
        {
            return Err(ToolError::Operation);
        }
        Ok(observation)
    };
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout_at(deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
    };
    let cleanup = tokio::time::timeout(Duration::from_secs(5), unit.stop()).await;
    if !matches!(cleanup, Ok(Ok(()))) {
        unit.kill_group();
    }
    let _ = child.start_kill();
    let reaped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let drained = drain_stderr(&mut stderr_task).await;
    if matches!(cleanup, Ok(Ok(()))) {
        permit
            .terminated()
            .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?;
        permit
            .cleanup()
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?;
    }
    if result.is_err()
        && let Some(failure) = drained.as_deref().and_then(bootstrap_failure)
    {
        return Err(failure);
    }
    if !matches!(cleanup, Ok(Ok(()))) {
        return Err(ToolError::GuardRejected(GuardStage::ProcessCleanup));
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected(GuardStage::LauncherReap));
    }
    let Some(stderr) = drained else {
        return Err(ToolError::GuardRejected(GuardStage::OutputEof));
    };
    if result.is_err() {
        let diagnostic = std::str::from_utf8(&stderr).unwrap_or_default();
        let stage = if diagnostic.contains("bwrap:") || diagnostic.contains("unshare:") {
            GuardStage::NamespaceBootstrap
        } else if diagnostic.contains("Failed to") {
            GuardStage::NativeManager
        } else {
            GuardStage::ControlProtocol
        };
        return Err(ToolError::GuardRejected(stage));
    }
    result
}

fn runtime_path(grants: &[super::super::toolchains::RuntimeGrant]) -> Result<String, ToolError> {
    let mut paths = grants
        .iter()
        .filter(|grant| grant.kind.is_toolchain())
        .map(|grant| {
            Ok(grant
                .mounted_path()?
                .join("bin")
                .to_str()
                .ok_or(ToolError::Path)?
                .to_owned())
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    paths.extend(["/usr/bin".into(), "/bin".into()]);
    Ok(paths.join(":"))
}

pub(super) fn verify_runtime_mounts(
    grants: &[super::super::toolchains::RuntimeGrant],
) -> Result<(), ToolError> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open("/proc/self/mountinfo")
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .take((MOUNTINFO_BYTES_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if bytes.len() > MOUNTINFO_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    let mounts = parse_mountinfo(&bytes)?;
    let targets = grants
        .iter()
        .map(super::super::toolchains::RuntimeGrant::mounted_path)
        .collect::<Result<Vec<_>, _>>()?;
    verify_mount_entries(&mounts, &targets)
}

fn verify_mount_entries(
    mounts: &[MountInfoEntry],
    targets: &[std::path::PathBuf],
) -> Result<(), ToolError> {
    let root = Path::new("/");
    if mounts
        .iter()
        .filter(|mount| mount.path.as_path() == root)
        .count()
        != 1
        || mounts
            .iter()
            .find(|mount| mount.path.as_path() == root)
            .is_none_or(|mount| mount.optional_fields != 0)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut unique_targets = std::collections::BTreeSet::new();
    for target in targets {
        if !unique_targets.insert(target) {
            return Err(ToolError::ProtectionUnavailable);
        }
        if mounts
            .iter()
            .filter(|mount| mount.path.as_path() == target.as_path())
            .count()
            != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let selected = mounts
            .iter()
            .find(|mount| mount.path.as_path() == target.as_path())
            .ok_or(ToolError::ProtectionUnavailable)?;
        if selected.optional_fields != 0
            || mounts.iter().any(|mount| {
                mount.path.as_path() != target.as_path() && mount.path.starts_with(target.as_path())
            })
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

struct MountInfoEntry {
    path: std::path::PathBuf,
    optional_fields: usize,
}

fn parse_mountinfo(bytes: &[u8]) -> Result<Vec<MountInfoEntry>, ToolError> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    if bytes.is_empty() || bytes.len() > MOUNTINFO_BYTES_CAP || bytes.last() != Some(&b'\n') {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut entries = Vec::new();
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty()
            || line.len() > MOUNTINFO_LINE_CAP
            || entries.len() >= MOUNTINFO_ENTRIES_CAP
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.iter().any(|field| field.is_empty()) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or(ToolError::ProtectionUnavailable)?;
        if separator < 6 || fields.len() != separator + 4 {
            return Err(ToolError::ProtectionUnavailable);
        }
        parse_decimal(fields[0])?;
        parse_decimal(fields[1])?;
        let device_separator = fields[2]
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(ToolError::ProtectionUnavailable)?;
        let (major, minor) = fields[2].split_at(device_separator);
        let minor = &minor[1..];
        parse_decimal(major)?;
        parse_decimal(minor)?;
        if !visible_ascii(fields[5])
            || !fields[6..separator]
                .iter()
                .all(|field| visible_ascii(field))
            || !visible_ascii(fields[separator + 1])
            || !visible_ascii(fields[separator + 3])
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let _mount_source = decode_mount_field(fields[separator + 2])?;
        let _mount_root = decode_mount_path(fields[3])?;
        let path = decode_mount_path(fields[4])?;
        entries.push(MountInfoEntry {
            path: std::path::PathBuf::from(OsString::from_vec(path)),
            optional_fields: separator - 6,
        });
    }
    if entries.is_empty() {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(entries)
}

fn parse_decimal(bytes: &[u8]) -> Result<u64, ToolError> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(ToolError::ProtectionUnavailable);
    }
    std::str::from_utf8(bytes)
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .parse()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

fn visible_ascii(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

fn decode_mount_field(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let escape = bytes
            .get(index + 1..index + 4)
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !escape.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let value = (escape[0] - b'0') * 64 + (escape[1] - b'0') * 8 + escape[2] - b'0';
        if !matches!(value, b' ' | b'\t' | b'\n' | b'\\') {
            return Err(ToolError::ProtectionUnavailable);
        }
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

fn decode_mount_path(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let path = decode_mount_field(bytes)?;
    if path.contains(&0)
        || !path.starts_with(b"/")
        || path.len() > 4096
        || path.len() > 1 && path.last() == Some(&b'/')
        || path != b"/"
            && path[1..]
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mountinfo_with_runtime(optional: &str, descendants: &[&str]) -> Vec<u8> {
        let mut bytes = format!(
            "1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache /runtime/toolchains/rust ro{optional} - ext4 /dev/root ro\n"
        )
        .into_bytes();
        for (index, path) in descendants.iter().enumerate() {
            bytes.extend_from_slice(
                format!(
                    "{} 2 8:1 /cache/{index} {path} ro - ext4 /dev/root ro\n",
                    index + 3
                )
                .as_bytes(),
            );
        }
        bytes
    }

    #[test]
    fn runtime_mount_attestation_requires_private_root_and_exact_leaf_mounts() {
        let target = Path::new("/runtime/toolchains/rust").to_owned();
        let valid = parse_mountinfo(&mountinfo_with_runtime("", &[])).unwrap();
        assert!(verify_mount_entries(&valid, std::slice::from_ref(&target)).is_ok());

        let shared = mountinfo_with_runtime(" shared:2", &[]);
        assert!(
            verify_mount_entries(
                &parse_mountinfo(&shared).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );

        let nested = mountinfo_with_runtime("", &["/runtime/toolchains/rust/nested"]);
        assert!(
            verify_mount_entries(
                &parse_mountinfo(&nested).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );

        let shared_root = b"1 0 8:1 / / rw shared:1 - ext4 /dev/root rw\n2 1 8:1 /cache /runtime/toolchains/rust ro - ext4 /dev/root ro\n";
        assert!(
            verify_mount_entries(
                &parse_mountinfo(shared_root).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );
    }

    #[test]
    fn mountinfo_decodes_only_kernel_path_escapes_and_rejects_ambiguous_records() {
        let target = Path::new("/runtime/toolchains/a b").to_owned();
        let escaped = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache/a\\040b /runtime/toolchains/a\\040b ro - ext4 /dev/root ro\n";
        let entries = parse_mountinfo(escaped).unwrap();
        assert!(verify_mount_entries(&entries, std::slice::from_ref(&target)).is_ok());

        let invalid_escape = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache/a\\141b /runtime/toolchains/a\\141b ro - ext4 /dev/root ro\n";
        assert!(parse_mountinfo(invalid_escape).is_err());

        let stacked = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache /runtime/toolchains/rust ro - ext4 /dev/root ro\n3 1 8:1 /other /runtime/toolchains/rust ro - ext4 /dev/root ro\n";
        assert!(
            verify_mount_entries(
                &parse_mountinfo(stacked).unwrap(),
                &[Path::new("/runtime/toolchains/rust").to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn selected_runtime_path_and_cache_mounts_are_exact() {
        use crate::tools::toolchains::RuntimeKind;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let node = home.join(".nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(node.join("bin")).unwrap();
        let executable = node.join("bin/node");
        std::fs::write(&executable, b"synthetic executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cargo_cache = home.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cargo_cache).unwrap();

        let candidates = crate::tools::toolchains::discover(&home).unwrap();
        let node = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::Node)
            .unwrap();
        let cache = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::CargoCache)
            .unwrap();
        let command_path = runtime_path(std::slice::from_ref(&node.grant)).unwrap();
        assert_eq!(
            command_path,
            format!(
                "{}:/usr/bin:/bin",
                node.grant.mounted_path().unwrap().join("bin").display()
            )
        );
        assert_eq!(
            cache.grant.mounted_path().unwrap(),
            Path::new("/scratch/cargo/registry/cache")
        );
        assert!(node.grant.verify_host().is_ok() && cache.grant.verify_host().is_ok());
    }

    #[tokio::test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native private mount propagation fixture; requires unprivileged user namespaces"
    )]
    async fn native_runtime_mount_attestation() {
        const MODE: &str = "ARANY_TEST_RUNTIME_MOUNT_MODE";
        if let Some(mode) = std::env::var_os(MODE) {
            use crate::tools::toolchains::RuntimeGrant;

            let grant_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_GRANT").unwrap();
            let result_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_RESULT").unwrap();
            let ready_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_READY").unwrap();
            let go_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_GO").unwrap();
            let grant: RuntimeGrant =
                serde_json::from_slice(&std::fs::read(grant_path).expect("fixture grant"))
                    .expect("fixture grant schema");
            let target = grant.mounted_path().unwrap();
            grant.verify_at(&target).expect("runtime alias identity");
            let call = ToolCall::Command {
                program: "node".into(),
                args: Vec::new(),
                cwd: String::new(),
            };
            let grants = std::slice::from_ref(&grant);
            let mut stage = super::super::BootstrapStage::Workspace;
            if super::super::attest_runtime_mounts(&call, grants, &mut stage).is_err() {
                let failure = super::super::BootstrapFailure { stage };
                std::fs::write(
                    result_path,
                    serde_json::to_vec(&failure).expect("bounded stage encoding"),
                )
                .expect("fixture failure result");
                return;
            }
            if mode == "exercise" {
                std::fs::write(ready_path, b"ready").expect("fixture Ready marker");
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !Path::new(&go_path).exists() {
                    assert!(std::time::Instant::now() < deadline, "fixture GO deadline");
                    std::thread::sleep(Duration::from_millis(10));
                }
                super::super::attest_runtime_mounts(&call, grants, &mut stage)
                    .expect("late shared-source mount must not propagate");
            }
            std::fs::write(result_path, b"PASS").expect("fixture success result");
            return;
        }

        use crate::tools::toolchains::{RuntimeKind, RuntimeManager};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("home/.nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir_all(source.join("late")).unwrap();
        std::fs::create_dir_all(source.join("nested")).unwrap();
        let node = source.join("bin/node");
        std::fs::write(&node, b"fixture runtime executable").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = fixture.path().join("runtime-alias");
        std::fs::create_dir(&alias).unwrap();
        let late_source = fixture.path().join("late-source");
        let nested_source = fixture.path().join("nested-source");
        std::fs::create_dir(&late_source).unwrap();
        std::fs::create_dir(&nested_source).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::Node,
            manager: RuntimeManager::Nvm,
            version: Some("v22.3.0".into()),
            path: source.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        grant.validate_shape().unwrap();
        let grant_path = fixture.path().join("grant.json");
        let target = grant.mounted_path().unwrap();
        let result_path = fixture.path().join("result");
        let test_binary = std::env::current_exe().unwrap();
        std::fs::write(fixture.path().join("arany-test-bin"), b"placeholder").unwrap();
        std::fs::write(&grant_path, serde_json::to_vec(&grant).unwrap()).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime-mounts.sh");
        let mut command = Command::new("/usr/bin/unshare");
        command
            .args([
                "--user",
                "--map-current-user",
                "--mount",
                "--propagation",
                "shared",
                "--keep-caps",
                "--",
                "/usr/bin/bash",
            ])
            .arg(script)
            .args([
                source.as_os_str(),
                alias.as_os_str(),
                late_source.as_os_str(),
                nested_source.as_os_str(),
                fixture.path().as_os_str(),
                test_binary.as_os_str(),
                target.as_os_str(),
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        let (status, stdout, stderr) = capture(
            command.spawn().map_err(|_| ToolError::Operation).unwrap(),
            64 * 1024,
            Duration::from_secs(40),
        )
        .await
        .unwrap();
        assert!(
            status.success(),
            "native mount attestation fixture: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(
            stdout,
            b"private-source-ok\nlate-mount-isolated\nnested-mount-rejected-at-runtime-stage\n"
        );
        assert_eq!(
            std::fs::read(result_path).unwrap(),
            b"{\"stage\":\"runtime_mounts\"}"
        );
    }

    #[tokio::test]
    #[ignore = "native Linux retained failed-unit recovery; requires systemd user manager and delegated cgroup v2"]
    async fn retained_failed_unit_is_reset_before_capacity_is_reclaimed() {
        check_available().unwrap();
        let id = uuid::Uuid::now_v7();
        let unit = Unit {
            name: format!("arany-tool-{id}.service"),
            invocation: None,
            group: None,
            kill: None,
        };
        let name = unit.name.clone();
        let result = tokio::spawn(async move {
            let mut command = manager_command("/usr/bin/systemd-run");
            command.args([
                "--user",
                "--no-ask-password",
                "--quiet",
                "--wait",
                "--service-type=exec",
                "--unit",
                &unit.name,
                "--property=Restart=no",
                "--property=KillMode=control-group",
                "--property=TimeoutStopSec=2s",
                "--property=RuntimeMaxSec=5s",
                "/usr/bin/false",
            ]);
            let (status, _, _) = capture(command.spawn().unwrap(), 4096, Duration::from_secs(10))
                .await
                .unwrap();
            assert!(!status.success());
            let failed = unit.properties().await.unwrap();
            assert_eq!(failed["ActiveState"], "failed");
            assert_eq!(failed["MainPID"], "0");
            assert_eq!(failed["ControlGroup"], "");
            assert!(recover_reservation(id).await.unwrap());
            let reset = unit.properties().await.unwrap();
            assert_ne!(reset["ActiveState"], "failed");
            assert_eq!(reset["MainPID"], "0");
            assert_eq!(reset["ControlGroup"], "");
        })
        .await;
        for action in ["stop", "reset-failed"] {
            let mut command = manager_command("/usr/bin/systemctl");
            command.args(["--user", "--no-ask-password", action, &name]);
            if let Ok(child) = command.spawn() {
                let _ = capture(child, 4096, MANAGER_DEADLINE).await;
            }
        }
        result.unwrap();
    }
}

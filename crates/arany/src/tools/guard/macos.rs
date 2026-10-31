use super::{
    BootstrapFailure, BootstrapStage, GUARD_CAPTURE_BYTES, GuardStage, Ready, TASK_BYTES_CAP,
    TEARDOWN_WAIT, Task, bootstrap_failure, drain_stderr, parse_ready, read_capped,
    read_line_capped, read_observation,
};
use crate::session::WorkspaceIdentity;
use crate::tools::Cancellation;
use crate::tools::executable::EXECUTABLES;
use crate::tools::guard::transfer::WORKER_PREPARATION_MS;
use crate::tools::types::{MAX_PROCESSES_OR_THREADS, MEMORY_BYTES};
use crate::tools::{EffectIntent, ToolCall, ToolError, ToolObservation, ToolRuntime};
use crate::validate::{MAX_PATH_BYTES, digest};

pub(super) fn boot_session() -> Result<uuid::Uuid, ToolError> {
    ffi::boot_session()
}

pub(super) async fn recover_abandoned(
    parent: &Path,
    attempts: &mut usize,
) -> Result<(), ToolError> {
    unit::recover_retained_roots_with_budget(parent, rustix::process::geteuid().as_raw(), attempts)
        .await
}

pub(super) fn private_recovered(parent: &Path, id: uuid::Uuid) -> Result<(), ToolError> {
    unit::private_recovered(parent, id)
}
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};

#[allow(unsafe_code)]
mod ffi;
pub(crate) mod fixture;
mod storage;
mod unit;

pub(in crate::tools) const RUNTIME_COMPAT_FILE: &str = "arany-runtime-compat.dylib";
pub(in crate::tools) const RUNTIME_COMPAT_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/arany-runtime-compat.dylib"));
const RUNTIME_COMPAT_BYTES_CAP: usize = 128 * 1024;

pub(super) fn configure_runtime_compat<'a>(
    command: &mut tokio::process::Command,
    source: impl Into<super::PayloadProgram<'a>>,
    args: &mut [std::ffi::OsString],
    resources: &Path,
) -> Result<(), ToolError> {
    let source = source.into();
    let program = source.definition();
    let library = resources.join(RUNTIME_COMPAT_FILE);
    let path = library.to_str().ok_or(ToolError::Path)?;
    if path.contains(':') {
        return Err(ToolError::ProtectionUnavailable);
    }
    let staged = crate::tools::fs::read_regular(
        crate::tools::fs::open_absolute(path)?,
        RUNTIME_COMPAT_BYTES_CAP,
    )?;
    if staged != RUNTIME_COMPAT_BYTES {
        return Err(ToolError::ChangedInput);
    }
    command.env("DYLD_INSERT_LIBRARIES", path);
    if matches!(source, super::PayloadProgram::Prefix(_)) {
        let script = args.last_mut().ok_or(ToolError::Configuration)?;
        super::prefix_runtime_compat_script(script, path)?;
    } else if program.executable == "/bin/sh" {
        super::prefix_shell_runtime_compat(args, path)?;
    }
    Ok(())
}

// Resolves once the payload's volume falls below the reserve, or when free space cannot be observed.
pub(in crate::tools) async fn disk_floor_reached(workspace: &Path) {
    let Ok(directory) = crate::tools::fs::open_directory(workspace) else {
        return;
    };
    let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
    loop {
        samples.tick().await;
        if !storage::free_bytes(&directory)
            .is_ok_and(|free| free >= crate::tools::types::GUARD_DISK_RESERVE_BYTES)
        {
            return;
        }
    }
}

const CACHE_ALIAS: &str = "cache-alias";

// Cargo fingerprints absolute registry and target paths, so a stable per-Workspace alias keeps a cloned cache fresh across units.
fn cache_alias(device: u64, inode: u64) -> PathBuf {
    Path::new(unit::UNIT_ROOT_PARENT).join(format!("arany-c-{device}-{inode}"))
}

// The alias is used only while it resolves to this unit's own view; the Workspace cache lease keeps other units from repointing it.
pub(in crate::tools) fn cache_view(root: &Path) -> PathBuf {
    let view = root.join("cache");
    std::fs::read_link(root.join(CACHE_ALIAS))
        .ok()
        .filter(|alias| std::fs::read_link(alias).is_ok_and(|target| target == view))
        .unwrap_or(view)
}

// Best effort: a name another user squats in the sticky parent leaves the unit on its private path.
pub(super) fn publish_cache_alias(root: &Path, device: u64, inode: u64) {
    let alias = cache_alias(device, inode);
    let staged = alias.with_extension(uuid::Uuid::now_v7().to_string());
    let published = std::os::unix::fs::symlink(root.join("cache"), &staged)
        .and_then(|()| std::fs::rename(&staged, &alias))
        .and_then(|()| std::os::unix::fs::symlink(&alias, root.join(CACHE_ALIAS)));
    if published.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
}

pub(super) fn retire_cache_alias(root: &Path, device: u64, inode: u64) {
    let alias = cache_alias(device, inode);
    if std::fs::read_link(&alias).is_ok_and(|target| target == root.join("cache")) {
        let _ = std::fs::remove_file(alias);
    }
}

pub(super) fn unit_main() -> std::process::ExitCode {
    unit::helper_main()
}

const CPU_LIMIT: Duration = Duration::from_secs(15);
const SYSTEM_PROGRAMS_CAP: usize = 2048;
const SYSTEM_PROGRAMS_BYTES_CAP: usize = 96 * 1024;
const PROFILE_BYTES_CAP: usize = 96 * 1024;
pub(super) const SUPERVISION_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const PREPARATION_DEADLINE: Duration = Duration::from_secs(30);
pub(in crate::tools) const RESERVATION_OVERHEAD: Duration =
    PREPARATION_DEADLINE.saturating_add(Duration::from_secs(5));
const PROCESS_LIMITS: &[(rustix::process::Resource, u64)] = &[
    (rustix::process::Resource::Nofile, 64),
    (rustix::process::Resource::Core, 0),
];
const SYS_FORK: i32 = 2;
const SYS_SETUID: i32 = 23;
const SYS_PTRACE: i32 = 26;
const SYS_SENDMSG: i32 = 28;
const SYS_SIGACTION: i32 = 46;
const SYS_SIGPROCMASK: i32 = 48;
const SYS_EXECVE: i32 = 59;
const SYS_VFORK: i32 = 66;
const SYS_SOCKET: i32 = 97;
const SYS_CONNECT: i32 = 98;
const SYS_BIND: i32 = 104;
const SYS_LISTEN: i32 = 106;
const SYS_SIGSUSPEND: i32 = 111;
const SYS_SETREUID: i32 = 126;
const SYS_SETREGID: i32 = 127;
const SYS_SENDTO: i32 = 133;
const SYS_SETGID: i32 = 181;
const SYS_SETEGID: i32 = 182;
const SYS_SETEUID: i32 = 183;
const SYS_SIGRETURN: i32 = 184;
const SYS_SETRLIMIT: i32 = 195;
const SYS_SYSCTL: i32 = 202;
const SYS_POSIX_SPAWN: i32 = 244;
const SYS_SYSCTLBYNAME: i32 = 274;
const SYS___PTHREAD_SIGMASK: i32 = 329;
const SYS___SIGWAIT: i32 = 330;
const SYS___DISABLE_THREADSIGNAL: i32 = 331;
const SYS___MAC_EXECVE: i32 = 380;
const SYS_SENDMSG_NOCANCEL: i32 = 402;
const SYS_CONNECT_NOCANCEL: i32 = 409;
const SYS_SIGSUSPEND_NOCANCEL: i32 = 410;
const SYS_SENDTO_NOCANCEL: i32 = 413;
const SYS___SIGWAIT_NOCANCEL: i32 = 422;
const SYS_CONNECTX: i32 = 447;
const SYS_SOCKET_DELEGATE: i32 = 450;
const SYS_COALITION: i32 = 458;
const SYS_COALITION_INFO: i32 = 459;
const SYS_SENDMSG_X: i32 = 481;
const SYS_PERSONA: i32 = 494;
const SYS_COALITION_POLICY_SET: i32 = 556;
const FILE_DENIED_SYSCALLS: &[i32] = &[
    SYS_FORK,
    SYS_SENDMSG,
    SYS_SIGACTION,
    SYS_SIGPROCMASK,
    SYS_EXECVE,
    SYS_VFORK,
    SYS_SOCKET,
    SYS_CONNECT,
    SYS_BIND,
    SYS_LISTEN,
    SYS_SIGSUSPEND,
    SYS_SENDTO,
    SYS_SIGRETURN,
    SYS_SETRLIMIT,
    SYS_SYSCTL,
    SYS_POSIX_SPAWN,
    SYS_SYSCTLBYNAME,
    SYS___PTHREAD_SIGMASK,
    SYS___SIGWAIT,
    SYS___DISABLE_THREADSIGNAL,
    SYS___MAC_EXECVE,
    SYS_SENDMSG_NOCANCEL,
    SYS_CONNECT_NOCANCEL,
    SYS_SIGSUSPEND_NOCANCEL,
    SYS_SENDTO_NOCANCEL,
    SYS___SIGWAIT_NOCANCEL,
    SYS_CONNECTX,
    SYS_SOCKET_DELEGATE,
    SYS_SENDMSG_X,
];
const COMMAND_DENIED_SYSCALLS: &[i32] = &[
    SYS_SETUID,
    SYS_PTRACE,
    SYS_SENDMSG,
    SYS_SOCKET,
    SYS_CONNECT,
    SYS_BIND,
    SYS_LISTEN,
    SYS_SETREUID,
    SYS_SETREGID,
    SYS_SENDTO,
    SYS_SETGID,
    SYS_SETEGID,
    SYS_SETEUID,
    SYS_SETRLIMIT,
    SYS_SYSCTL,
    SYS_SYSCTLBYNAME,
    SYS___SIGWAIT,
    SYS___DISABLE_THREADSIGNAL,
    SYS___MAC_EXECVE,
    SYS_SENDMSG_NOCANCEL,
    SYS_CONNECT_NOCANCEL,
    SYS_SIGSUSPEND_NOCANCEL,
    SYS_SENDTO_NOCANCEL,
    SYS_CONNECTX,
    SYS_SOCKET_DELEGATE,
    SYS_COALITION,
    SYS_COALITION_INFO,
    SYS_SENDMSG_X,
    SYS_PERSONA,
    SYS_COALITION_POLICY_SET,
];

// Socket creation and sends are admitted only because the profile's destination filter limits them to the proxy port; bind, listen and descriptor delegation stay denied.
const PROXY_COMMAND_DENIED_SYSCALLS: &[i32] = &[
    SYS_SETUID,
    SYS_PTRACE,
    SYS_BIND,
    SYS_LISTEN,
    SYS_SETREUID,
    SYS_SETREGID,
    SYS_SETGID,
    SYS_SETEGID,
    SYS_SETEUID,
    SYS_SETRLIMIT,
    SYS_SYSCTL,
    SYS_SYSCTLBYNAME,
    SYS___SIGWAIT,
    SYS___DISABLE_THREADSIGNAL,
    SYS___MAC_EXECVE,
    SYS_SIGSUSPEND_NOCANCEL,
    SYS_SOCKET_DELEGATE,
    SYS_COALITION,
    SYS_COALITION_INFO,
    SYS_SENDMSG_X,
    SYS_PERSONA,
    SYS_COALITION_POLICY_SET,
];

const PROFILE: &str = "(allow file-read-metadata (literal \"/\") (literal \"/System\") (literal \"/usr\") (literal \"/dev\"))
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))
";

// The loader opens /, so this grants root metadata only, nothing beneath it.
// System LibreSSL exits when its denied openssl.cnf open returns EPERM, so its public TLS configuration stays readable.
const COMMAND_PROFILE: &str = "(allow file-read-metadata (literal \"/\") (literal \"/System\") (literal \"/usr\") (literal \"/dev\"))
(allow file-read-metadata (literal \"/System/Cryptexes\") (literal \"/System/Cryptexes/OS\") (literal \"/var\") (literal \"/etc\") (literal \"/private/var\") (literal \"/private/var/select\") (literal \"/private/var/select/sh\"))
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/\") (literal \"/bin\") (literal \"/usr/bin\") (literal \"/dev/random\") (literal \"/dev/urandom\"))
(allow file-read* (subpath \"/private/etc/ssl\"))
(allow file-read* file-write* (literal \"/dev/null\"))
(allow process-fork)
(deny process-fork (require-entitlement \"com.apple.private.coalition-spawn\") (require-entitlement \"com.apple.private.persona-mgmt\"))
(deny process-exec-update-label)
(allow signal (target self) (target children))
";

const PROXY_RULES: &str = "(allow system-socket (socket-domain AF_INET))
(allow network-outbound (remote ip \"localhost:{port}\"))
";

fn profile(rules: &str, denied_syscalls: &[i32]) -> String {
    let mut profile =
        String::from("(version 1)\n(deny default)\n(allow syscall-unix)\n(deny syscall-unix");
    for syscall in denied_syscalls {
        profile.push_str(&format!(" (syscall-number {syscall})"));
    }
    profile.push_str(")\n");
    profile.push_str(rules);
    profile
}

pub(super) fn check_available() -> Result<(), ToolError> {
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        return Err(ToolError::ProtectionUnavailable);
    }
    for program in ["/bin/launchctl", "/usr/bin/env"] {
        if !system_program(program)? {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    unit::check_available()
}

const MACOS_REWIND_CONTRACT: &[u8] = b";rewind-handover-framed-v1-fixed-capacity-non-spawning;bulk-preparation-at-most-120s-under-action-ceiling";

const MACOS_SELECTION_CONTRACT: &[u8] = b";definition-parser-task-bytes-only-no-workspace-grant;command-selection-v2-ignore-0.4.33-frozen-controls-32k-256k-256-4096-lines4096-expanded64k-1m-stars16-recursive4-braces4-calls8388608-command-runtime-from-select-ready-pinned-runtime-report1s-uptime-raw-action-deadline-capture-cpu-delta-phase-only-selection-limits-no-post-capture-sampling-memory512m-wait30s-input-exclusions-before-open-oversize-omission-full-output-validation-filesystem-cause-select-errors-v2-snapshot-files16384-bytes134217728-scan131072-depth32-path-metadata16777216-project-storage268435456";

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut bytes = b"arany-macos-guard-v18:shared-fifo-capacity;owner-only-same-device-unit-directories;boot-bound-locked-root-recovery;persisted-supervisor-before-task;no-mount-under-unit-root;no-buffered-go-before-ready;typed-files-pinned-skills;file-no-fork-exec;closed-inherited-fds;live-no-follow-handles;metadata-only-directory-ancestors;workspace-call-write;guard-read-only;file-cpu-default-unblocked-immutable;supervised-resources;one-use-launchd-coalition;authenticated-private-control;atomic-audit-token-signals;owned-unit-cleanup;exact-child-kill-reap;strict-bounded-no-follow-command-changeset-after-coalition-emptiness-before-root-removal;bounded-go-receipt;versioned-exact-ro-runtime-and-cache-grants;admitted-cloned-cache-directory-grant-stable-alias-metadata-literal;mount-table-no-descendant-under-selected-physical-runtime-before-seatbelt-and-before-go;command-project-default268435456".to_vec();
    bytes.extend_from_slice(&super::super::output::DRAIN_BYTES.to_le_bytes());
    bytes.extend_from_slice(MACOS_REWIND_CONTRACT);
    bytes.extend_from_slice(MACOS_SELECTION_CONTRACT);
    bytes.extend_from_slice(b";user-programs-v1:selected-subtrees-and-homebrew-prefixes-and-manager-roots-read-map-exec-without-system-catalogue-checks;pinned-roots-mount-topology-before-seatbelt-and-ready-go");
    bytes.extend_from_slice(&super::super::discovery::enforcement_contract());
    bytes.extend_from_slice(b";runtime-compat-v1:public-vm-page-size-sysconf-getpagesize;non-page-sysconf-forward;fixed-cleared-dyld-env;known-pinned-sh-c-fixed-quoted-export-reject-option-script-position;no-sysctl-policy-exception;read-only-nofollow-exact-bytes;128KiB-runtime-budget");
    bytes.extend_from_slice(RUNTIME_COMPAT_BYTES);
    bytes.extend_from_slice(include_bytes!("macos/runtime_compat.c"));
    bytes.extend_from_slice(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/build.rs"
    )));
    for kind in [ProfileKind::File, ProfileKind::Command] {
        bytes.extend_from_slice(SeatbeltProfile::new(kind).text.as_bytes());
    }
    bytes.extend_from_slice(b"native-commands-mcp:checked-private-cwd;disk-floor-supervised-project-scratch;fresh-selected-snapshot;read-only-pinned-runtime;bounded-positive-root-owned-non-setid-unentitled-system-exec;stamp-bound-process-signature-verdicts;admission-catalogue-bound-to-enforcement;per-worker-pinned-system-directories-no-follow-metadata-revalidation;exact-literal-union-profile;dynamic-coalition-persona-entitlement-fork-denial;null-bootstrap-registered-ports;child-only-signals;supervised-cpu;physical-footprint-memory");
    bytes.extend_from_slice(&CPU_LIMIT.as_nanos().to_le_bytes());
    bytes.extend_from_slice(&SUPERVISION_INTERVAL.as_nanos().to_le_bytes());
    bytes.extend_from_slice(&MEMORY_BYTES.to_le_bytes());
    bytes.extend_from_slice(&MAX_PROCESSES_OR_THREADS.to_le_bytes());
    bytes.extend_from_slice(&crate::tools::types::GUARD_DISK_RESERVE_BYTES.to_le_bytes());
    bytes.extend_from_slice(&(SYSTEM_PROGRAMS_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(SYSTEM_PROGRAMS_BYTES_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(PROFILE_BYTES_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(TASK_BYTES_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(crate::tools::types::MAX_CHANGESET_FILES as u64).to_le_bytes());
    bytes.extend_from_slice(&(crate::tools::types::MAX_CHANGESET_BYTES as u64).to_le_bytes());
    bytes.extend_from_slice(
        &(crate::tools::types::MAX_CHANGESET_SERIALIZED_BYTES as u64).to_le_bytes(),
    );
    bytes.extend_from_slice(
        &(crate::tools::types::MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES as u64).to_le_bytes(),
    );
    for &(resource, limit) in PROCESS_LIMITS {
        bytes.extend_from_slice(&(resource as u32).to_le_bytes());
        bytes.extend_from_slice(&limit.to_le_bytes());
    }
    Ok(digest(&bytes))
}

pub(super) fn bootstrap() -> Result<(), ToolError> {
    if ffi::task_info(std::process::id())?.pti_threadnum != 1 {
        return Err(ToolError::ProtectionUnavailable);
    }
    // This single-threaded helper owns inherited descriptors before Rust creates any handles.
    close_inherited_descriptors()?;
    configure_resources()
}

fn close_inherited_descriptors() -> Result<(), ToolError> {
    for descriptor in ffi::open_descriptors()? {
        if descriptor >= 3 {
            nix::unistd::close(descriptor).map_err(|_| ToolError::ProtectionUnavailable)?;
        }
    }
    Ok(())
}

fn configure_resources() -> Result<(), ToolError> {
    use rustix::process::{Rlimit, setrlimit};
    for &(resource, limit) in PROCESS_LIMITS {
        setrlimit(
            resource,
            Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    Ok(())
}

fn configure_file_cpu() -> Result<(), ToolError> {
    if !ffi::reset_cpu_signal() {
        return Err(ToolError::ProtectionUnavailable);
    }
    use rustix::process::{Rlimit, setrlimit};
    setrlimit(
        rustix::process::Resource::Cpu,
        Rlimit {
            current: Some(CPU_LIMIT.as_secs()),
            maximum: Some(CPU_LIMIT.as_secs()),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    Ok(())
}

fn quoted(path: &Path) -> Result<String, ToolError> {
    let path = path.to_str().ok_or(ToolError::Path)?;
    if !path.starts_with('/') || path.len() > MAX_PATH_BYTES || path.chars().any(char::is_control) {
        return Err(ToolError::Path);
    }
    serde_json::to_string(path).map_err(|_| ToolError::Path)
}

#[derive(Clone, Copy)]
enum ProfileKind {
    File,
    Command,
    CommandProxy,
}

impl ProfileKind {
    fn denied_syscalls(self) -> &'static [i32] {
        match self {
            Self::File => FILE_DENIED_SYSCALLS,
            Self::Command => COMMAND_DENIED_SYSCALLS,
            Self::CommandProxy => PROXY_COMMAND_DENIED_SYSCALLS,
        }
    }

    fn is_file_like(self) -> bool {
        matches!(self, Self::File)
    }
}

// A case-insensitive POSIX regex alternation of exactly the names `validate::forbidden` accepts.
fn protected_pattern() -> String {
    let letters = |name: &str| {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphabetic() {
                    format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
                } else {
                    format!("[{c}]")
                }
            })
            .collect::<String>()
    };
    crate::validate::FORBIDDEN_NAMES
        .iter()
        .map(|name| letters(name))
        .chain(std::iter::once(format!("{}[^/]*", letters(".env."))))
        .collect::<Vec<_>>()
        .join("|")
}

struct SeatbeltProfile {
    text: String,
    kind: ProfileKind,
    proxy_port: Option<u16>,
    masked: Vec<PathBuf>,
}

impl SeatbeltProfile {
    fn new(kind: ProfileKind) -> Self {
        let rules = match kind {
            ProfileKind::File => PROFILE.to_owned(),
            ProfileKind::Command | ProfileKind::CommandProxy => COMMAND_PROFILE.to_owned(),
        };
        Self {
            text: profile(&rules, kind.denied_syscalls()),
            kind,
            proxy_port: None,
            masked: Vec::new(),
        }
    }

    fn allow_proxy(&mut self, port: u16) {
        self.text
            .push_str(&PROXY_RULES.replace("{port}", &port.to_string()));
        self.proxy_port = Some(port);
    }

    fn rule(&mut self, operations: &str, filter: &str, path: &Path) -> Result<(), ToolError> {
        self.text
            .push_str(&format!("({operations} ({filter} {}))\n", quoted(path)?));
        Ok(())
    }

    fn grant(&mut self, operations: &str, filter: &str, path: &Path) -> Result<(), ToolError> {
        for directory in path.ancestors().skip(1) {
            self.rule("allow file-read-metadata", "literal", directory)?;
        }
        self.rule(operations, filter, path)
    }

    fn runtime_grant(
        &mut self,
        grant: &crate::tools::toolchains::RuntimeGrant,
    ) -> Result<(), ToolError> {
        use crate::tools::toolchains::RuntimeKind;

        grant.verify_host()?;
        if grant.kind == RuntimeKind::AppleCompiler {
            self.grant(
                "allow file-read* file-map-executable",
                "subpath",
                &grant.path,
            )?;
            for executable in grant.apple_compiler_executables()? {
                self.grant("allow process-exec", "literal", &executable)?;
            }
        } else {
            let operations = if grant.kind.is_host_mounted() {
                "allow file-read* file-map-executable process-exec"
            } else {
                "allow file-read*"
            };
            self.grant(operations, "subpath", &grant.path)?;
            if let Some(root) = &grant.root {
                self.grant(operations, "subpath", &root.path)?;
            }
            if grant.kind == RuntimeKind::ExtraRead {
                self.masked.push(grant.path.clone());
            }
        }
        Ok(())
    }

    // Seatbelt applies the last matching rule, so these denies follow every allow.
    fn mask_protected(&mut self) -> Result<(), ToolError> {
        let names = protected_pattern();
        for root in std::mem::take(&mut self.masked) {
            let root = quoted(&root)?;
            let root = &root[1..root.len() - 1];
            if root.contains(['\\', '"']) {
                return Err(ToolError::Path);
            }
            let escaped: String = root
                .chars()
                .map(|c| {
                    if "^$.|?*+()[]{}".contains(c) {
                        format!("\\{c}")
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            self.text.push_str(&format!(
                "(deny file-read* file-map-executable process-exec (regex #\"^{escaped}/(.*/)?({names})(/.*)?$\"))\n"
            ));
        }
        Ok(())
    }

    fn protect_guard(&mut self, executable: &Path) -> Result<(), ToolError> {
        self.rule("deny file-write*", "literal", executable)
    }

    fn install(self) -> Result<(), ToolError> {
        let operations: &[(&CStr, i32)] = match self.kind {
            ProfileKind::File => &[(c"process-fork", 1), (c"network-outbound", 1)],
            ProfileKind::Command => &[
                (c"process-fork", 0),
                (c"network-outbound", 1),
                (c"mach-lookup", 1),
                (c"iokit-open-user-client", 1),
                (c"process-exec-update-label", 1),
            ],
            ProfileKind::CommandProxy => &[
                (c"process-fork", 0),
                (c"mach-lookup", 1),
                (c"iokit-open-user-client", 1),
                (c"process-exec-update-label", 1),
            ],
        };
        if !self.kind.is_file_like() && !ffi::detach_bootstrap_ports() {
            return Err(ToolError::ProtectionUnavailable);
        }
        install_profile(self.text)?;
        attest_policy(operations, self.kind.denied_syscalls())?;
        if let Some(port) = self.proxy_port {
            attest_proxy_routes(port)?;
        }
        if self.kind.is_file_like() {
            verify_cpu_controls()?;
        }
        Ok(())
    }
}

// Filtered allows leave the unfiltered verdict ambiguous, so the route is attested by connects the profile must refuse before they leave the host.
fn attest_proxy_routes(port: u16) -> Result<(), ToolError> {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    const ATTESTATION_DEADLINE: Duration = Duration::from_secs(1);
    let denied = |address: SocketAddr| {
        matches!(
            TcpStream::connect_timeout(&address, ATTESTATION_DEADLINE),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        )
    };
    let other = port.wrapping_add(1).max(1);
    if denied(SocketAddr::from((Ipv4Addr::LOCALHOST, other)))
        && denied(SocketAddr::from(([192, 0, 2, 1], 443)))
    {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn proxy_contract() -> String {
    format!(
        "{}{PROXY_RULES}",
        SeatbeltProfile::new(ProfileKind::CommandProxy).text
    )
}

fn install_profile(profile: String) -> Result<(), ToolError> {
    if profile.len() > PROFILE_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    let profile = CString::new(profile).map_err(|_| ToolError::Path)?;
    if !ffi::install_sandbox(&profile) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(super) fn install(
    task: &Task,
    stage: &mut super::BootstrapStage,
) -> Result<PathBuf, ToolError> {
    if task.intent.call.is_process() {
        return install_command(task, stage);
    }
    configure_file_cpu()?;
    let mut profile = SeatbeltProfile::new(ProfileKind::File);
    if !matches!(task.intent.call, ToolCall::ParseAgentDefinition { .. }) {
        profile.grant("allow file-read*", "subpath", &task.workspace)?;
    }
    profile.protect_guard(&task.guard_executable)?;
    profile.install()?;
    Ok(PathBuf::from("/"))
}

fn unit_root(task: &Task) -> Result<PathBuf, ToolError> {
    let root = std::env::current_dir().map_err(|_| ToolError::Path)?;
    crate::private_fs::open_directory(&root, false).map_err(|_| ToolError::Path)?;
    crate::tools::fs::reject_overlap(&root, &task.workspace)?;
    if crate::tools::fs::identity(&crate::tools::fs::open_directory(&task.workspace)?)?
        != (task.intent.workspace_device, task.intent.workspace_inode)
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(root)
}

fn command_root(task: &Task) -> Result<PathBuf, ToolError> {
    let root = unit_root(task)?;
    storage::verify_storage(&root, &task.intent.limits)?;
    Ok(root)
}

pub(super) fn system_program(path: &str) -> Result<bool, ToolError> {
    Ok(system_program_metadata(path)?.is_some())
}

fn system_program_metadata(path: &str) -> Result<Option<std::fs::Metadata>, ToolError> {
    if !crate::tools::config::valid_absolute(path) {
        return Err(ToolError::Path);
    }
    let path = Path::new(path);
    if !matches!(path.parent(), Some(parent) if EXECUTABLES.system_directories.iter().any(|directory| parent == Path::new(directory)))
    {
        return Ok(None);
    }
    let parent = system_program_directory(path.parent().ok_or(ToolError::Path)?)?;
    Ok(Some(system_program_metadata_in(&parent, path)?))
}

fn system_program_directory(path: &Path) -> Result<cap_std::fs::Dir, ToolError> {
    use cap_std::fs::MetadataExt;
    let parent = crate::tools::fs::open_directory(path)?;
    let metadata = parent.dir_metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_dir() || !system_directory(metadata.uid(), metadata.mode()) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(parent)
}

fn system_program_metadata_in(
    parent: &cap_std::fs::Dir,
    path: &Path,
) -> Result<std::fs::Metadata, ToolError> {
    use std::os::unix::fs::MetadataExt;
    let file = crate::tools::fs::open_file(
        parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(ToolError::Path)?,
    )
    .map_err(|_| ToolError::Path)?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !crate::tools::fs::macos_system_file(
        metadata.is_file(),
        metadata.uid(),
        metadata.nlink(),
        metadata.mode(),
    ) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(metadata)
}

fn system_directory(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

fn unprivileged_helper() -> Result<(), ToolError> {
    if rustix::process::geteuid().as_raw() == 0
        || rustix::process::getuid() != rustix::process::geteuid()
        || rustix::process::getgid() != rustix::process::getegid()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn configured_system_programs(task: &Task) -> Result<Vec<PathBuf>, ToolError> {
    let mut required = Vec::new();
    for program in task.config.commands.iter().chain(
        task.config
            .mcp
            .iter()
            .filter_map(|server| server.program.as_ref()),
    ) {
        program.verify()?;
        if system_program(&program.executable)? {
            required.push(PathBuf::from(&program.executable));
        }
    }
    Ok(required)
}

fn install_command(task: &Task, stage: &mut super::BootstrapStage) -> Result<PathBuf, ToolError> {
    use std::os::unix::fs::MetadataExt;

    unprivileged_helper()?;
    let root = command_root(task)?;
    let mut required_system_programs = configured_system_programs(task)?;
    let mut profile = SeatbeltProfile::new(match task.proxy_port {
        Some(_) => ProfileKind::CommandProxy,
        None => ProfileKind::Command,
    });
    if let Some(port) = task.proxy_port {
        profile.allow_proxy(port);
    }
    let grants = if task.intent.limits.cache_bytes > 0 {
        &["workspace", "scratch", "cache"][..]
    } else {
        &["workspace", "scratch"][..]
    };
    for name in grants {
        let path = match (&task.editor_tree, *name) {
            (Some(tree), "workspace") => tree.clone(),
            _ => root.join(name),
        };
        profile.grant(
            "allow file-read* file-write* file-map-executable process-exec",
            "subpath",
            &path,
        )?;
    }
    for grant in &task.config.runtime_grants {
        profile.runtime_grant(grant)?;
    }
    let view = cache_view(&root);
    if task.intent.limits.cache_bytes > 0 && view != root.join("cache") {
        profile.rule(
            "allow file-read-metadata",
            "literal",
            &root.join(CACHE_ALIAS),
        )?;
        profile.rule("allow file-read-metadata", "literal", &view)?;
    }
    let resources = root.join("runtime");
    let metadata = std::fs::symlink_metadata(&resources).map_err(|_| ToolError::Path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    profile.rule(
        "allow file-read* file-map-executable",
        "subpath",
        &resources,
    )?;
    profile.rule("allow process-exec", "subpath", &resources.join("programs"))?;
    task.system_programs
        .as_ref()
        .ok_or(ToolError::ProtectionUnavailable)?
        .allow(&mut profile.text, &mut required_system_programs)?;
    if !required_system_programs.is_empty() {
        return Err(ToolError::ProtectionUnavailable);
    }
    *stage = super::BootstrapStage::RuntimeMounts;
    verify_runtime_mounts(&task.config.runtime_grants)?;
    *stage = super::BootstrapStage::Capabilities;
    profile.protect_guard(&task.guard_executable)?;
    profile.mask_protected()?;
    profile.install()?;
    Ok(root)
}

pub(super) fn verify_runtime_mounts(
    grants: &[crate::tools::toolchains::RuntimeGrant],
) -> Result<(), ToolError> {
    let mut roots = Vec::new();
    for grant in grants {
        grant.verify_host()?;
        if grant.kind == crate::tools::toolchains::RuntimeKind::AppleCompiler {
            let [_, clang, ld] = grant.apple_compiler_executables()?;
            for executable in [clang, ld] {
                if ffi::unentitled_code(&executable)? != Some(true) {
                    return Err(ToolError::ProtectionUnavailable);
                }
            }
        }
        for path in std::iter::once(&grant.path).chain(grant.root.iter().map(|root| &root.path)) {
            roots.push(std::fs::canonicalize(path).map_err(|_| ToolError::ProtectionUnavailable)?);
        }
    }
    let mounts = ffi::mounts()?;
    if mounts.iter().any(|(_, target)| {
        !target.is_absolute()
            || target.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || roots
                .iter()
                .any(|root| target != root && target.starts_with(root))
    }) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemPrograms {
    // Compact stamps keep the catalogue inside the shared Task byte cap.
    programs: Vec<(String, [u64; 7])>,
}

fn program_stamp(metadata: &std::fs::Metadata) -> [u64; 7] {
    use std::os::unix::fs::MetadataExt;
    [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

pub(crate) fn macos_enforcement_digest(
    programs: Option<&SystemPrograms>,
) -> Result<[u8; 32], ToolError> {
    let policy = enforcement_digest()?;
    let Some(programs) = programs else {
        return Ok(policy);
    };
    if programs.programs.len() > SYSTEM_PROGRAMS_CAP {
        return Err(ToolError::Limit);
    }
    let mut bytes = policy.to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(programs).map_err(|_| ToolError::Configuration)?);
    if bytes.len() > SYSTEM_PROGRAMS_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    Ok(digest(&bytes))
}

impl SystemPrograms {
    pub(crate) fn admit() -> Result<Self, ToolError> {
        use cap_std::fs::MetadataExt;

        let capture = fixture::Span::begin(fixture::Stage::Catalogue);
        let mut count = 0;
        let mut programs = Vec::new();
        for directory in EXECUTABLES.system_directories {
            let dir = crate::tools::fs::open_directory(Path::new(directory))?;
            let metadata = dir
                .dir_metadata()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if !system_directory(metadata.uid(), metadata.mode()) {
                return Err(ToolError::ProtectionUnavailable);
            }
            for entry in dir
                .entries()
                .map_err(|_| ToolError::ProtectionUnavailable)?
            {
                count += 1;
                if count > SYSTEM_PROGRAMS_CAP {
                    return Err(ToolError::Limit);
                }
                let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
                if !entry
                    .file_type()
                    .map_err(|_| ToolError::ProtectionUnavailable)?
                    .is_file()
                {
                    continue;
                }
                let metadata = entry
                    .metadata()
                    .map_err(|_| ToolError::ProtectionUnavailable)?;
                if !crate::tools::fs::macos_system_file(
                    metadata.is_file(),
                    metadata.uid(),
                    metadata.nlink(),
                    metadata.mode(),
                ) {
                    continue;
                }
                let path = Path::new(directory).join(entry.file_name());
                let path = path.to_str().ok_or(ToolError::Path)?;
                let stamp = program_stamp(
                    &system_program_metadata(path)?.ok_or(ToolError::ProtectionUnavailable)?,
                );
                if !unentitled_program(path, stamp)? {
                    continue;
                }
                programs.push((path.to_owned(), stamp));
            }
        }
        programs.sort_unstable();
        let programs = Self { programs };
        macos_enforcement_digest(Some(&programs))?;
        capture.complete();
        Ok(programs)
    }

    fn allow(&self, profile: &mut String, required: &mut Vec<PathBuf>) -> Result<(), ToolError> {
        if self.programs.is_empty() || self.programs.len() > SYSTEM_PROGRAMS_CAP {
            return Err(ToolError::ProtectionUnavailable);
        }
        let directories = EXECUTABLES
            .system_directories
            .iter()
            .map(|directory| {
                let path = Path::new(directory);
                Ok((path, system_program_directory(path)?))
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        profile.push_str("(allow file-read* file-map-executable process-exec (require-any\n");
        for (path, expected) in &self.programs {
            if !crate::tools::config::valid_absolute(path) {
                return Err(ToolError::Path);
            }
            let path = Path::new(path);
            let directory = directories
                .iter()
                .find(|(directory, _)| Some(*directory) == path.parent())
                .ok_or(ToolError::ProtectionUnavailable)?;
            let metadata = system_program_metadata_in(&directory.1, path)?;
            if program_stamp(&metadata) != *expected {
                return Err(ToolError::ChangedInput);
            }
            required.retain(|required| required != path);
            profile.push_str(&format!("(literal {})\n", quoted(path)?));
            if profile.len() > PROFILE_BYTES_CAP {
                return Err(ToolError::Limit);
            }
        }
        profile.push_str("))\n");
        if profile.len() > PROFILE_BYTES_CAP {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

// Signature evaluation dominates admission; reuse only determined verdicts for an identical stamp.
fn unentitled_program(path: &str, stamp: [u64; 7]) -> Result<bool, ToolError> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    type Verdicts = HashMap<String, ([u64; 7], bool)>;
    static VERDICTS: LazyLock<Mutex<Verdicts>> = LazyLock::new(Mutex::default);
    let cached = VERDICTS
        .lock()
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .get(path)
        .and_then(|&(cached, verdict)| (cached == stamp).then_some(verdict));
    if let Some(verdict) = cached {
        return Ok(verdict);
    }
    let verdict = ffi::unentitled_code(Path::new(path))?;
    let after = system_program_metadata(path)?.ok_or(ToolError::ProtectionUnavailable)?;
    if program_stamp(&after) != stamp {
        return Err(ToolError::ChangedInput);
    }
    let Some(verdict) = verdict else {
        return Ok(false);
    };
    let mut verdicts = VERDICTS
        .lock()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if verdicts.len() >= SYSTEM_PROGRAMS_CAP && !verdicts.contains_key(path) {
        verdicts.clear();
    }
    verdicts.insert(path.to_owned(), (stamp, verdict));
    Ok(verdict)
}

fn attest_policy(operations: &[(&CStr, i32)], denied_syscalls: &[i32]) -> Result<(), ToolError> {
    if operations
        .iter()
        .any(|&(operation, expected)| ffi::sandbox_verdict(operation) != expected)
        || !denied_syscalls
            .iter()
            .all(|&syscall| ffi::syscall_denied(syscall))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_cpu_controls() -> Result<(), ToolError> {
    use rustix::process::getrlimit;
    for &(resource, limit) in PROCESS_LIMITS {
        let actual = getrlimit(resource);
        if actual.current != Some(limit) || actual.maximum != Some(limit) {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    let cpu = getrlimit(rustix::process::Resource::Cpu);
    if cpu.current != Some(CPU_LIMIT.as_secs()) || cpu.maximum != Some(CPU_LIMIT.as_secs()) {
        return Err(ToolError::ProtectionUnavailable);
    }
    if !ffi::cpu_signal_changes_denied() {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn check_process_resources(pid: u32) -> Result<bool, ToolError> {
    let (info, usage) = match ffi::task_info(pid).and_then(|info| Ok((info, ffi::rusage(pid)?))) {
        Ok(sample) => sample,
        Err(_) if ffi::exited(pid) => return Ok(false),
        Err(_) => return Err(ToolError::ProtectionUnavailable),
    };
    // Resident size omits compressed and swapped pages; the footprint does not.
    if info.pti_threadnum < 1
        || usage.ri_phys_footprint > MEMORY_BYTES
        || info.pti_threadnum as u32 > MAX_PROCESSES_OR_THREADS
    {
        return Err(ToolError::Limit);
    }
    Ok(true)
}

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: Cancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    // Dropping the awaiting future stops only this worker, never the caller's Run.
    struct CancelOnDrop(Cancellation);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let worker = CancelOnDrop(Cancellation::new());
    let worker_cancellation = worker.0.clone();
    let runtime = runtime.clone();
    let intent = intent.clone();
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let reactor = tokio::runtime::Handle::current();
    let mut handle = tokio::task::spawn_blocking(move || {
        crate::tools::fs::validate_workspace(&runtime)?;
        // Admission and Engine deadlines must use the same runtime clock.
        reactor.block_on(unit::execute(
            &runtime,
            &intent,
            worker_cancellation,
            &worker_started,
        ))
    });
    let result = tokio::select! {
        result = &mut handle => result,
        () = cancellation.cancelled() => {
            worker.0.cancel();
            handle.await
        }
    }
    .unwrap_or_else(|_| {
        Err(super::execution_failure(
            super::GuardCheck::ControllerFailed,
        ))
    });
    *dispatched = started.load(Ordering::Acquire);
    result
}

impl Task {
    pub(super) fn new(
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        cache: Option<super::BuildCacheTransfer>,
    ) -> Result<Self, ToolError> {
        let config = runtime.process_config(&intent.call)?;
        Ok(Self {
            command_deadline: None,
            policy_grants: matches!(config, std::borrow::Cow::Owned(_))
                .then(|| runtime.config.runtime_grants.clone()),
            config: config.into_owned(),
            intent: intent.clone(),
            cache,
            hook_stdin: runtime.hook_stdin.as_deref().map(str::to_owned),
            secret: None,
            hidden: runtime.hidden_paths(&intent.call),
            definition: runtime.definitions.payload(&intent.call)?,
            handover: runtime
                .handover
                .as_ref()
                .and_then(|_| super::transfer::handover_ref(intent)),
            workspace: runtime.workspace.clone(),
            editor_tree: runtime.editor_tree(intent),
            guard_executable: runtime.guard_executable.clone(),
            system_programs: if intent.call.is_process() {
                runtime.system_programs.as_deref().cloned()
            } else {
                None
            },
            proxy_port: None,
        })
    }

    fn into_runtime(
        self,
        worker_input: Option<Vec<u8>>,
    ) -> (ToolRuntime, EffectIntent, Option<super::BuildCacheTransfer>) {
        let cache = self.cache;
        let handover = self
            .handover
            .as_ref()
            .and(worker_input.as_ref())
            .map(|bytes| Arc::from(bytes.as_slice()));
        let runtime = ToolRuntime {
            host_settings_admitted: false,
            host_settings_saved: Default::default(),
            guard_available: false,
            guard_mode: crate::tools::GuardMode::Auto,
            once_reads: Default::default(),
            git_environment: None,
            git_hooks: None,
            repository: None,
            human_approvals: false,
            handover,
            definitions: super::super::definitions::DefinitionSnapshots::from_payload(
                &self.intent.call,
                self.definition,
            )
            .into(),
            permissions: None,
            result_filter: Some(self.hidden),
            unguarded_rules: None,
            context_limit_bytes: std::sync::Arc::new(
                crate::tools::types::MAX_TOOL_CONTEXT_BYTES.into(),
            ),
            queue_progress: None,
            active_action_gate: None,
            hook_stdin: self.hook_stdin.map(Into::into),
            checkpoint_gate: None,
            cache_state: None,
            command_tools: Default::default(),
            command_environment: std::sync::Arc::new(crate::tools::CommandEnvironment::empty()),
            spill_access: None,
            config: self.config,
            workspace: self.workspace,
            reads: None,
            workspace_identity: WorkspaceIdentity {
                device: self.intent.workspace_device,
                inode: self.intent.workspace_inode,
            },
            guard_executable: self.guard_executable,
            enforcement_digest: self.intent.enforcement_digest,
            process_enforcement_digest: self.intent.enforcement_digest,
            system_programs: self.system_programs.map(Arc::new),
        };
        (runtime, self.intent, cache)
    }
}

async fn worker_ready(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    task: &[u8],
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Ready, ToolError> {
    tokio::time::timeout(
        (PREPARATION_DEADLINE + super::transfer::transfer_time(intent))
            .min(Duration::from_millis(u64::from(WORKER_PREPARATION_MS))),
        async {
            stdin
                .write_all(task)
                .await
                .map_err(|_| ToolError::Operation)?;
            let mut bytes = read_line_capped(stdout, 2048).await?;
            if bytes == b"WORKER_READY\n" {
                let input = runtime
                    .handover
                    .as_deref()
                    .ok_or(ToolError::Configuration)?;
                super::transfer::send_frame(stdin, input).await?;
                bytes = read_line_capped(stdout, 2048).await?;
            }
            parse_ready(&bytes, intent)
        },
    )
    .await
    .map_err(|_| ToolError::GuardBootstrap(BootstrapFailure::at(BootstrapStage::Handshake)))?
}

struct WorkerOutput {
    observation: ToolObservation,
    windows: Option<crate::tools::output::CommandWindows>,
}

async fn read_command_windows(
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    intent: &EffectIntent,
) -> Result<Option<crate::tools::output::CommandWindows>, ToolError> {
    if matches!(intent.call, ToolCall::Command { .. })
        && tokio::io::AsyncBufReadExt::fill_buf(stdout)
            .await
            .map_err(|_| ToolError::Operation)?
            .first()
            == Some(&b'W')
    {
        if read_line_capped(stdout, 8).await? != b"WINDOWS\n" {
            return Err(ToolError::Operation);
        }
        let bytes =
            read_line_capped(stdout, 8 * intent.limits.result_bytes as usize + 1024).await?;
        let windows: crate::tools::output::CommandWindows =
            serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
        if !windows.valid(intent.limits.result_bytes as usize) {
            return Err(ToolError::Operation);
        }
        Ok(Some(windows))
    } else {
        Ok(None)
    }
}

async fn worker_receipt(
    mut stdin: tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    intent: &EffectIntent,
) -> Result<WorkerOutput, ToolError> {
    let capture = fixture::Span::begin(fixture::Stage::WorkerReceipt);
    stdin
        .write_all(b"GO\n")
        .await
        .map_err(|_| ToolError::Operation)?;
    drop(stdin);
    let windows = read_command_windows(stdout, intent).await?;
    let observation = read_observation(stdout, intent).await?;
    capture.complete();
    Ok(WorkerOutput {
        observation,
        windows,
    })
}

type WorkerPlacement<'a> = (
    &'a Path,
    Option<[u8; 8]>,
    Option<u16>,
    Option<&'a Path>,
    Option<Vec<crate::tools::toolchains::RuntimeGrant>>,
);

struct WorkerLink<'a> {
    scope: &'a mut unit::Scope,
    control: &'a mut tokio::net::unix::OwnedWriteHalf,
    receive_go: tokio::sync::oneshot::Receiver<()>,
}

fn worker_task(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    (_, command_deadline, proxy_port, editor_tree, policy_grants): WorkerPlacement<'_>,
) -> Result<Vec<u8>, ToolError> {
    crate::tools::fs::validate_workspace(runtime)?;
    let mut task = Task::new(runtime, intent, None)?;
    task.policy_grants = policy_grants;
    task.editor_tree = editor_tree.map(Path::to_owned);
    task.command_deadline = command_deadline;
    task.proxy_port = proxy_port;
    super::encode_task(&task)
}

async fn execute_worker(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: Cancellation,
    link: WorkerLink<'_>,
    placement: WorkerPlacement<'_>,
) -> Result<WorkerOutput, ToolError> {
    let WorkerLink {
        scope,
        control,
        receive_go,
    } = link;
    let root = placement.0;
    let task = worker_task(runtime, intent, placement)?;
    let mut child = super::launcher_command(&runtime.guard_executable)
        .arg("--internal-tool-guard")
        .env_clear()
        .current_dir(if intent.call.is_process() {
            root
        } else {
            Path::new("/")
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let pid = child.id().ok_or(ToolError::ProtectionUnavailable)?;
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, GUARD_CAPTURE_BYTES));
    let (result, status) = {
        let execution = async {
            let capture = fixture::Span::begin(fixture::Stage::WorkerReady);
            let ready = worker_ready(runtime, intent, &task, &mut stdin, &mut stdout).await?;
            if ready.pid != pid {
                return Err(ToolError::ProtectionUnavailable);
            }
            if !check_process_resources(pid)
                .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?
            {
                return Err(ToolError::GuardRejected(GuardStage::ResourceAttestation));
            }
            let ready = serde_json::to_vec(&ready).map_err(|_| ToolError::Operation)?;
            control
                .write_all(&ready)
                .await
                .map_err(|_| ToolError::Operation)?;
            control
                .write_all(b"\n")
                .await
                .map_err(|_| ToolError::Operation)?;
            capture.complete();
            let capture = fixture::Span::begin(fixture::Stage::WorkerGo);
            receive_go.await.map_err(|_| ToolError::Operation)?;
            capture.complete();
            let payload_deadline = super::payload_report_deadline(
                tokio::time::Instant::now(),
                intent.limits.runtime_ms,
            );
            tokio::time::timeout_at(payload_deadline, worker_receipt(stdin, &mut stdout, intent))
                .await
                .map_err(|_| ToolError::Limit)?
        };
        let deadline = tokio::time::Instant::now()
            + RESERVATION_OVERHEAD
            + Duration::from_millis(u64::from(intent.limits.runtime_ms))
            + super::transfer::transfer_time(intent);
        supervise_worker(
            tokio::time::timeout_at(deadline, execution),
            &mut child,
            &mut cancellation,
            scope,
        )
        .await
    };
    settle_worker(&mut child, status, &mut stderr_task, scope, result).await
}

async fn supervise_worker(
    execution: tokio::time::Timeout<impl Future<Output = Result<WorkerOutput, ToolError>>>,
    child: &mut tokio::process::Child,
    cancellation: &mut Cancellation,
    scope: &mut unit::Scope,
) -> (
    Result<WorkerOutput, ToolError>,
    Option<Result<std::process::ExitStatus, ToolError>>,
) {
    tokio::pin!(execution);
    let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
    let mut status = None;
    let result = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break Err(ToolError::Operation),
            result = &mut execution => break result.unwrap_or(Err(ToolError::Limit)),
            exited = child.wait(), if status.is_none() => {
                status = Some(exited.map_err(|_| ToolError::Operation));
            }
            _ = samples.tick() => {
                if let Err(error) = scope.check_resources() { break Err(error); }
            }
        }
    };
    (result, status)
}

// Reaps the launcher; a bootstrap failure it reported outranks the payload's own error.
async fn settle_worker(
    child: &mut tokio::process::Child,
    status: Option<Result<std::process::ExitStatus, ToolError>>,
    stderr_task: &mut tokio::task::JoinHandle<Result<Vec<u8>, ToolError>>,
    scope: &mut unit::Scope,
    result: Result<WorkerOutput, ToolError>,
) -> Result<WorkerOutput, ToolError> {
    if result.is_err() {
        let _ = child.start_kill();
    }
    let deadline = tokio::time::Instant::now() + TEARDOWN_WAIT;
    scope.teardown = Some(deadline);
    let capture = fixture::Span::begin(fixture::Stage::WorkerReap);
    let reaped = match status {
        Some(status) => Ok(status),
        None => {
            tokio::time::timeout_at(deadline, async {
                child.wait().await.map_err(|_| ToolError::Operation)
            })
            .await
        }
    };
    if reaped.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout_at(deadline, child.wait()).await;
    }
    let drained = drain_stderr(stderr_task, deadline).await;
    capture.finish(matches!(reaped, Ok(Ok(_))) && drained.is_some());
    if result.is_err()
        && let Some(failure) = drained.as_deref().and_then(bootstrap_failure)
    {
        return Err(failure);
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected(GuardStage::LauncherReap));
    }
    if result.is_ok() && !matches!(reaped, Ok(Ok(status)) if status.success()) {
        return Err(ToolError::Operation);
    }
    if drained.is_none() {
        return Err(ToolError::GuardRejected(GuardStage::OutputEof));
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_view_uses_the_stable_alias_only_while_it_resolves_to_this_unit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("unit");
        std::fs::create_dir_all(root.join("cache")).unwrap();
        assert_eq!(super::cache_view(&root), root.join("cache"));
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&alias, root.join(super::CACHE_ALIAS)).unwrap();
        std::os::unix::fs::symlink(root.join("cache"), &alias).unwrap();
        assert_eq!(super::cache_view(&root), alias);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
        assert_eq!(super::cache_view(&root), root.join("cache"));
    }

    #[test]
    fn enforcement_prose_matches_resource_limits() {
        use crate::validate::hex_digest;

        assert_eq!(
            hex_digest(MACOS_REWIND_CONTRACT),
            "6417dc6f02793f6d31d0e1b108b019982e2f49b74d5b23b23fab35a1780df097"
        );
        assert_eq!(
            hex_digest(MACOS_SELECTION_CONTRACT),
            "daf8d84c308a2bed54df0548b1badb17f3004f3cc4ee96c4299b973fde8c93f6"
        );
        super::super::tests::assert_enforcement_prose(
            MACOS_SELECTION_CONTRACT,
            MACOS_REWIND_CONTRACT,
        );
    }

    use super::*;

    #[test]
    fn proxy_profile_admits_one_loopback_port_and_keeps_bind_and_listen_denied() {
        let mut profile = SeatbeltProfile::new(ProfileKind::CommandProxy);
        let offline = SeatbeltProfile::new(ProfileKind::Command);
        assert!(!offline.text.contains("network-outbound"));
        assert!(!profile.text.contains("network-outbound"));
        profile.allow_proxy(49152);
        assert!(
            profile
                .text
                .contains("(allow network-outbound (remote ip \"localhost:49152\"))")
        );
        assert_eq!(profile.text.matches("(allow network-outbound").count(), 1);
        for denied in [SYS_BIND, SYS_LISTEN, SYS_SOCKET_DELEGATE, SYS_SENDMSG_X] {
            assert!(PROXY_COMMAND_DENIED_SYSCALLS.contains(&denied));
        }
        for admitted in [SYS_SOCKET, SYS_CONNECT, SYS_SENDTO, SYS_CONNECTX] {
            assert!(!PROXY_COMMAND_DENIED_SYSCALLS.contains(&admitted));
            assert!(COMMAND_DENIED_SYSCALLS.contains(&admitted));
        }
    }

    use crate::test_process::BoundedOutput;
    use crate::tools::config::Config;
    use crate::tools::types::ToolLimits;
    use nix::libc;
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn profile_builder_emits_exact_rules() {
        for (kind, rules, syscalls, operations, filter, expected) in [
            (
                ProfileKind::File,
                PROFILE,
                FILE_DENIED_SYSCALLS,
                "allow file-read*",
                "subpath",
                r#"(allow file-read-metadata (literal "/a"))
(allow file-read-metadata (literal "/"))
(allow file-read* (subpath "/a/b\"c"))
(deny file-write* (literal "/guard/arany"))
"#,
            ),
            (
                ProfileKind::Command,
                COMMAND_PROFILE,
                COMMAND_DENIED_SYSCALLS,
                "allow file-read* file-write* file-map-executable process-exec",
                "subpath",
                r#"(allow file-read-metadata (literal "/a"))
(allow file-read-metadata (literal "/"))
(allow file-read* file-write* file-map-executable process-exec (subpath "/a/b\"c"))
(deny file-write* (literal "/guard/arany"))
"#,
            ),
        ] {
            let mut actual = SeatbeltProfile::new(kind);
            assert_eq!(actual.text, profile(rules, syscalls));
            let base_len = actual.text.len();
            actual
                .grant(operations, filter, Path::new(r#"/a/b"c"#))
                .unwrap();
            actual.protect_guard(Path::new("/guard/arany")).unwrap();
            assert_eq!(&actual.text.as_bytes()[base_len..], expected.as_bytes());
        }
    }

    fn runtime_fixture(
        home: &Path,
        kind: crate::tools::toolchains::RuntimeKind,
        manager: crate::tools::toolchains::RuntimeManager,
        directory: &str,
        root: Option<&str>,
    ) -> crate::tools::toolchains::RuntimeGrant {
        use crate::tools::toolchains::{GrantRoot, RuntimeGrant};

        let identity = |path: &Path| {
            std::fs::create_dir_all(path).unwrap();
            crate::tools::fs::identity(&crate::tools::fs::open_directory(path).unwrap()).unwrap()
        };
        let path = home.join(directory);
        let (device, inode) = identity(&path);
        RuntimeGrant {
            kind,
            manager,
            version: None,
            path,
            device,
            inode,
            root: root.map(|suffix| {
                let path = home.join(suffix);
                let (device, inode) = identity(&path);
                GrantRoot {
                    path,
                    device,
                    inode,
                }
            }),
            masks: Vec::new(),
        }
    }

    #[test]
    fn user_program_profiles_grant_only_selected_subtrees_and_reject_root_replacement() {
        use crate::tools::toolchains::{RuntimeKind, RuntimeManager};

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        for (kind, manager, directory, root, operations) in [
            (
                RuntimeKind::UserPrograms,
                RuntimeManager::UserDirectory,
                ".local/bin",
                None,
                "allow file-read* file-map-executable process-exec",
            ),
            (
                RuntimeKind::UserPrograms,
                RuntimeManager::UserDirectory,
                "brew/bin",
                Some("brew"),
                "allow file-read* file-map-executable process-exec",
            ),
            (
                RuntimeKind::ManagerShims,
                RuntimeManager::Rustup,
                ".cargo/bin",
                Some(".rustup"),
                "allow file-read* file-map-executable process-exec",
            ),
            (
                RuntimeKind::CargoCache,
                RuntimeManager::HostCache,
                ".cargo/registry/cache",
                None,
                "allow file-read*",
            ),
        ] {
            let grant = runtime_fixture(home, kind, manager, directory, root);
            let path = &grant.path;
            let mut profile = SeatbeltProfile::new(ProfileKind::Command);
            let start = profile.text.len();
            profile.runtime_grant(&grant).unwrap();
            let actual = profile.text[start..]
                .lines()
                .filter(|line| !line.starts_with("(allow file-read-metadata "))
                .collect::<Vec<_>>();
            let mut expected = vec![format!(
                "({operations} (subpath {}))",
                quoted(path).unwrap()
            )];
            if let Some(root) = &grant.root {
                expected.push(format!(
                    "({operations} (subpath {}))",
                    quoted(&root.path).unwrap()
                ));
            }
            assert_eq!(actual, expected, "{directory}");
            verify_runtime_mounts(std::slice::from_ref(&grant)).unwrap();
            if let Some(root) = &grant.root {
                std::fs::rename(&root.path, root.path.with_extension("old")).unwrap();
                std::fs::create_dir(&root.path).unwrap();
                assert!(matches!(
                    profile.runtime_grant(&grant),
                    Err(ToolError::ChangedInput)
                ));
                assert!(matches!(
                    verify_runtime_mounts(&[grant]),
                    Err(ToolError::ChangedInput)
                ));
            }
        }
    }

    #[test]
    fn system_catalogue_revalidates_pins_and_binds_the_profile() {
        let programs = SystemPrograms::admit().unwrap();
        let mut required = vec![PathBuf::from("/bin/sh"), PathBuf::from("/usr/bin/true")];
        let mut profile = String::new();
        programs.allow(&mut profile, &mut required).unwrap();
        assert!(required.is_empty());
        assert!(
            !programs
                .programs
                .iter()
                .any(|(path, _)| path == "/usr/bin/sudo")
        );
        let bytes = serde_json::to_vec(&programs).unwrap();
        let mut admitted: SystemPrograms = serde_json::from_slice(&bytes).unwrap();
        let enforcement = macos_enforcement_digest(Some(&admitted)).unwrap();
        assert_eq!(
            enforcement,
            macos_enforcement_digest(Some(&programs)).unwrap()
        );
        assert_ne!(enforcement, enforcement_digest().unwrap());
        for case in ["stale", "outside", "noncanonical", "overflow"] {
            admitted.programs = programs.programs.clone();
            match case {
                "stale" => admitted.programs[0].1[1] ^= 1,
                "outside" => admitted.programs[0].0 = "/etc/passwd".into(),
                "noncanonical" => admitted.programs[0].0 = "/usr/bin/./true".into(),
                "overflow" => admitted
                    .programs
                    .resize(SYSTEM_PROGRAMS_CAP + 1, programs.programs[0].clone()),
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    admitted.allow(&mut String::new(), &mut Vec::new()),
                    Err(ToolError::ChangedInput
                        | ToolError::ProtectionUnavailable
                        | ToolError::Path)
                ),
                "{case}"
            );
            if case == "overflow" {
                assert!(matches!(
                    macos_enforcement_digest(Some(&admitted)),
                    Err(ToolError::Limit)
                ));
            } else {
                assert_ne!(
                    enforcement,
                    macos_enforcement_digest(Some(&admitted)).unwrap(),
                    "{case}"
                );
            }
        }
    }

    fn probe(cpu: bool) -> std::process::Output {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        std::fs::write(
            temp.path().join("workspace/selected.txt"),
            b"synthetic input",
        )
        .unwrap();
        std::fs::write(temp.path().join("outside"), b"synthetic excluded data").unwrap();
        let network = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::guard::macos::tests::native_non_spawning_profile_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("ARANY_TEST_NON_SPAWNING_PROFILE_ROOT", temp.path())
            .env(
                "ARANY_TEST_NON_SPAWNING_PROFILE_PORT",
                network.local_addr().unwrap().port().to_string(),
            )
            .env(
                "ARANY_TEST_NON_SPAWNING_PROFILE_CPU",
                if cpu { "1" } else { "0" },
            )
            .current_dir(temp.path())
            .bounded_output_for(Duration::from_secs(90), 32 * 1024)
            .unwrap()
    }

    #[test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(arany_guard, ignore = "requires local test IPC")]
    fn native_non_spawning_profile_denies_writes_network_spawn_and_cpu_changes() {
        let output = probe(false);
        assert!(
            output.status.success(),
            "native non-spawning profile: {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::test_process::assert_single_test_completed(
            &output.stdout,
            "native non-spawning profile",
        );
        assert!(output.stderr.is_empty());
    }

    #[test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(
        any(arany_guard, not(arany_ci)),
        ignore = "native 15-second CPU expiry; finite isolated process"
    )]
    fn native_non_spawning_profile_cpu_expiry_cannot_be_disabled() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let output = probe(true);
        assert_eq!(output.status.signal(), Some(libc::SIGXCPU));
        assert!(String::from_utf8_lossy(&output.stdout).contains("CPU readiness"));
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[ignore = "isolated native non-spawning profile helper"]
    fn native_non_spawning_profile_helper() {
        let root =
            std::env::var_os("ARANY_TEST_NON_SPAWNING_PROFILE_ROOT").expect("private fixture root");
        let root = Path::new(&root);
        let port = std::env::var("ARANY_TEST_NON_SPAWNING_PROFILE_PORT")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let cpu = std::env::var("ARANY_TEST_NON_SPAWNING_PROFILE_CPU").unwrap() == "1";
        let workspace = root.join("workspace");
        let (device, inode) =
            crate::tools::fs::identity(&crate::tools::fs::open_directory(&workspace).unwrap())
                .unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}))
        .unwrap();
        let task = Task {
            policy_grants: None,
            command_deadline: None,
            definition: None,
            handover: None,
            intent: EffectIntent {
                host: None,
                wrap: None,
                rewrite: Vec::new(),
                host_command: None,
                target: crate::session::worker::EffectTarget::Workspace,
                id: uuid::Uuid::now_v7(),
                run_id: crate::session::RunId::default(),
                agent_run_id: crate::session::AgentRunId::default(),
                policy_digest: config.digest(),
                enforcement_digest: enforcement_digest().unwrap(),
                workspace_device: device,
                workspace_inode: inode,
                // Probe the retired primary intent directly: native installation must grant no Workspace writes.
                call: ToolCall::Write {
                    path: "synthetic".into(),
                    expected_digest: None,
                    content: "synthetic".into(),
                },
                limits: ToolLimits::default(),
                use_count: 1,
            },
            config,
            cache: None,
            hook_stdin: None,
            secret: None,
            hidden: Default::default(),
            workspace,
            editor_tree: None,
            guard_executable: std::env::current_exe().unwrap(),
            system_programs: None,
            proxy_port: None,
        };
        assert!(Path::new("/usr/bin/true").is_file());
        configure_resources().unwrap();
        let mut stage = super::BootstrapStage::Workspace;
        install(&task, &mut stage).unwrap();
        assert_eq!(
            ffi::process_arguments_errno(std::process::id() as i32),
            Some(libc::EPERM)
        );
        assert_eq!(
            std::fs::read(root.join("outside")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(crate::tools::fs::open_directory(&task.workspace).is_ok());
        assert_eq!(
            std::fs::read(task.workspace.join("selected.txt")).unwrap(),
            b"synthetic input"
        );
        assert_eq!(
            std::fs::write(task.workspace.join("synthetic"), b"forbidden")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::metadata(root.join("outside")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_dir(root).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_dir("/private/tmp").unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::write(root.join("outside"), b"forbidden")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        // macOS 15 reports denied binds and execs as ENOENT; later releases report EPERM.
        assert!(matches!(
            std::os::unix::net::UnixListener::bind(root.join("workspace/socket"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
        assert_eq!(
            std::fs::symlink_metadata(root.join("workspace/socket"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(matches!(
            std::process::Command::new("/usr/bin/true")
                .status()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
        if cpu {
            std::io::stdout().write_all(b"CPU readiness\n").unwrap();
            std::io::stdout().flush().unwrap();
            loop {
                std::hint::spin_loop();
            }
        }
    }
}

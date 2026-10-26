use arany::test_support::process::BoundedOutput;
use nix::sched::{CloneFlags, unshare};
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::thread::{self, CapabilitySet, CapabilitySets};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const CAPTURE_BYTES: usize = 32 * 1024;
const FILE_BYTES: u64 = 16 * 1024;
const EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
const READ_BUFFER_BYTES: usize = 8192;
const TMPFS_BYTES: u64 = 64 * 1024;
const TMPFS_INODES: u64 = 16;
const DEADLINE: Duration = Duration::from_secs(10);
const LAUNCHER: &str = "/usr/bin/unshare";
const ENTRY_REACHED: &str = "namespace probe entry reached";
const ENTRY_ARGS: &[&str] = &[
    "--user",
    "--map-current-user",
    "--mount",
    "--propagation",
    "private",
    "--keep-caps",
    "--",
];

pub(super) fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [mode, root, descriptor] if mode == "--in-process" || mode == "--after-unshare" => {
            admit_inherited_executable(descriptor)?;
            let result = entry(mode == "--in-process", Path::new(root));
            if result.is_err() {
                snapshot("entry failure")?;
            }
            result
        }
        [mode] if mode == "--verify-clear" => verify_clear(),
        [] => compare_entries(),
        _ => Err("run without arguments to compare both entries".into()),
    }
}

fn admit_inherited_executable(descriptor: &std::ffi::OsStr) -> Result<()> {
    let descriptor: RawFd = descriptor
        .to_str()
        .ok_or("invalid executable descriptor")?
        .parse()?;
    if descriptor <= std::io::stderr().as_raw_fd() {
        return Err("executable descriptor must be separate from stdio".into());
    }
    let inherited = fs::metadata(format!("/proc/self/fd/{descriptor}"))?;
    let running = fs::metadata("/proc/self/exe")?;
    if (inherited.dev(), inherited.ino()) != (running.dev(), running.ino()) {
        return Err("inherited descriptor does not identify the running probe".into());
    }
    nix::unistd::close(descriptor)?;
    Ok(())
}

fn bounded_text(path: &str) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_BYTES {
        return Err("probe metadata bound exceeded".into());
    }
    Ok(String::from_utf8(bytes)?)
}

fn executable_identity(path: &str) -> Result<()> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.mode() & 0o6022 != 0
        || (path == LAUNCHER && metadata.uid() != 0)
    {
        return Err(
            "probe executable must be regular, non-set-ID and not group/other writable".into(),
        );
    }
    let mut hash = Sha256::new();
    let mut reader = (&mut file).take(EXECUTABLE_BYTES + 1);
    let mut total = 0;
    let mut buffer = [0; READ_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        hash.update(&buffer[..count]);
    }
    if total > EXECUTABLE_BYTES {
        return Err("probe executable bound exceeded".into());
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "executable={path:?} dev={} ino={} uid={} sha256={digest}",
        metadata.dev(),
        metadata.ino(),
        metadata.uid()
    );
    Ok(())
}

fn snapshot(stage: &str) -> Result<()> {
    println!("stage={stage} pid={}", std::process::id());
    println!(
        "/proc/self/attr/current={:?}",
        bounded_text("/proc/self/attr/current")
    );
    for path in ["/proc/self/uid_map", "/proc/self/gid_map"] {
        println!("{path}={:?}", bounded_text(path)?);
    }
    for line in bounded_text("/proc/self/status")?.lines() {
        if line.starts_with("Cap") || line.starts_with("NoNewPrivs:") {
            println!("{line}");
        }
    }
    Ok(())
}

fn host_evidence() -> Result<bool> {
    if nix::unistd::getuid().is_root() {
        return Err("run as an unprivileged disposable account, without sudo".into());
    }
    let release = bounded_text("/etc/os-release")?;
    let restriction = bounded_text("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    let apparmor = bounded_text("/sys/module/apparmor/parameters/enabled");
    println!("os-release={release:?}");
    println!("kernel={:?}", bounded_text("/proc/sys/kernel/osrelease")?);
    println!("AppArmor restriction={restriction:?}");
    println!("AppArmor enabled={apparmor:?}");
    snapshot("parent before either entry")?;
    executable_identity("/proc/self/exe")?;
    executable_identity(LAUNCHER)?;
    for args in [
        vec![
            "-W",
            "-f=${Package} ${Version}\n",
            "util-linux",
            "apparmor",
            "bubblewrap",
        ],
        vec!["-S", "/etc/apparmor.d/unshare"],
    ] {
        let output = Command::new("/usr/bin/dpkg-query")
            .args(args)
            .env_clear()
            .bounded_output_for(DEADLINE, CAPTURE_BYTES);
        println!("package/profile provenance={output:?}");
    }
    println!(
        "launcher AppArmor profile={:?}",
        bounded_text("/etc/apparmor.d/unshare")
    );
    Ok(release.lines().any(|line| line == "ID=ubuntu")
        && release.lines().any(|line| line == "VERSION_ID=\"24.04\"")
        && restriction.as_ref().is_ok_and(|value| value.trim() == "1")
        && apparmor.as_ref().is_ok_and(|value| value.trim() == "Y")
        && bounded_text("/proc/self/attr/current").is_ok_and(|value| value.trim() == "unconfined"))
}

fn compare_entries() -> Result<()> {
    let stock = host_evidence()?;
    let root = tempfile::tempdir()?;
    let mountpoint = root.path().join("mount");
    fs::create_dir(&mountpoint)?;
    // A parent procfs executable link is inaccessible after entering a child user namespace.
    let executable = nix::unistd::dup(File::open("/proc/self/exe")?)?;
    let pinned = format!("/proc/self/fd/{}", executable.as_raw_fd());
    let mut passed = [false; 2];
    for (index, in_process) in [true, false].into_iter().enumerate() {
        let mut command = Command::new(if in_process { &pinned } else { LAUNCHER });
        if !in_process {
            command.args(ENTRY_ARGS).arg(&pinned);
        }
        command
            .arg(if in_process {
                "--in-process"
            } else {
                "--after-unshare"
            })
            .arg(&mountpoint)
            .arg(executable.as_raw_fd().to_string())
            .current_dir(root.path())
            .env_clear();
        println!("entry argv={command:?}");
        let output = command.bounded_output_for(DEADLINE, CAPTURE_BYTES)?;
        println!(
            "exit={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let reached = output
            .stdout
            .split(|byte| *byte == b'\n')
            .any(|line| line == ENTRY_REACHED.as_bytes());
        if !reached {
            println!("startup failed before the probe snapshot; no mount/capability observation");
        }
        assert!(
            fs::read_dir(&mountpoint)?.next().is_none(),
            "no mount or file reached the host"
        );
        println!("positive teardown: entry reaped; host mountpoint empty");
        passed[index] = output.status.success() && reached;
    }
    if !stock {
        return Err(
            "diagnostic results only: stock Ubuntu 24.04/AppArmor prerequisites not established"
                .into(),
        );
    }
    match passed {
        [true, _] => println!("selected entry: in-process (D7 preference)"),
        [false, true] => println!("selected entry: distro unshare --keep-caps exec"),
        _ => {
            return Err("both stock entries failed; retain Draft and raise D7 to the owner".into());
        }
    }
    Ok(())
}

fn entry(in_process: bool, root: &Path) -> Result<()> {
    snapshot("probe entry (after launcher exec when selected)")?;
    executable_identity("/proc/self/exe")?;
    println!("{ENTRY_REACHED}");
    if in_process {
        let uid = nix::unistd::getuid();
        let gid = nix::unistd::getgid();
        unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)?;
        fs::write("/proc/self/setgroups", "deny")?;
        fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n"))?;
        fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))?;
    }
    snapshot("before private propagation and mount")?;
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )?;
    let options = std::ffi::CString::new(format!(
        "size={TMPFS_BYTES},nr_inodes={TMPFS_INODES},mode=0700"
    ))?;
    rustix::mount::mount(
        "tmpfs",
        root,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        options.as_c_str(),
    )?;
    let sentinel = root.join("sentinel");
    fs::write(&sentinel, b"namespace probe")?;
    if fs::read(&sentinel)? != b"namespace probe" {
        return Err("mounted sentinel content mismatch".into());
    }
    rustix::mount::unmount(root, UnmountFlags::empty())?;
    if sentinel.try_exists()? {
        return Err("sentinel remains visible after unmount".into());
    }
    println!("actual tmpfs mount/write/read/unmount succeeded");
    clear_capabilities()?;
    verify_clear()?;
    let mount = rustix::mount::mount("tmpfs", root, "tmpfs", MountFlags::empty(), None);
    if mount != Err(rustix::io::Errno::PERM) {
        return Err(format!("expected EPERM after capability removal, observed {mount:?}").into());
    }
    Err(Command::new("/proc/self/exe")
        .arg("--verify-clear")
        .env_clear()
        .exec()
        .into())
}

fn clear_capabilities() -> Result<()> {
    let last: u32 = bounded_text("/proc/sys/kernel/cap_last_cap")?
        .trim()
        .parse()?;
    if last >= u64::BITS {
        return Err("unsupported capability width".into());
    }
    for bit in 0..=last {
        thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << bit))?;
    }
    thread::clear_ambient_capability_set()?;
    thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )?;
    thread::set_no_new_privs(true)?;
    Ok(())
}

fn verify_clear() -> Result<()> {
    snapshot("capability-free before any bwrap fork")?;
    for line in bounded_text("/proc/self/status")?
        .lines()
        .filter(|line| line.starts_with("Cap"))
    {
        let (_, value) = line.split_once(':').ok_or("invalid capability status")?;
        if u64::from_str_radix(value.trim(), 16)? != 0 {
            return Err(format!("capability set not cleared: {line}").into());
        }
    }
    if !thread::no_new_privs()? {
        return Err("no-new-privileges is not set".into());
    }
    Ok(())
}

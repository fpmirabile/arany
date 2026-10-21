use crate::tools::ToolError;
use nix::libc;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

// Linux UAPI <linux/landlock.h>, stable since ABI 1 (Linux 5.13).
const CREATE_RULESET_VERSION: libc::c_long = 1;
const RULE_PATH_BENEATH: libc::c_long = 1;
const WRITE_FILE: u64 = 1 << 1;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;

// `struct landlock_ruleset_attr` prefix: the kernel reads exactly the size passed with it.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

// `struct landlock_path_beneath_attr`, declared `__attribute__((packed))` in the UAPI header.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

// Landlock and seccomp bind the thread and its descendants, so the Guard thread keeps its access.
pub(crate) fn spawn_confined(
    command: &mut tokio::process::Command,
    writable: &[&Path],
) -> Result<tokio::process::Child, ToolError> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _runtime = runtime.enter();
                restrict_thread(writable)?;
                command.spawn().map_err(|_| ToolError::Operation)
            })
            .join()
            .map_err(|_| ToolError::Operation)?
    })
}

fn restrict_thread(writable: &[&Path]) -> Result<(), ToolError> {
    // SAFETY: with a null attribute, zero size and the version flag the call only returns the
    // supported ABI number; it reads and writes no memory.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    // ABI 3 (Linux 6.2) is the first that mediates truncation; older kernels fail closed.
    if abi < 3 {
        return Err(ToolError::ProtectionUnavailable);
    }
    let handled = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM
        | REFER
        | TRUNCATE;
    let attr = RulesetAttr {
        handled_access_fs: handled,
    };
    // SAFETY: `attr` is a live, initialized `RulesetAttr` and the size passed is exactly its size.
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            std::mem::size_of::<RulesetAttr>(),
            0 as libc::c_long,
        )
    };
    let ruleset = i32::try_from(ruleset).map_err(|_| ToolError::ProtectionUnavailable)?;
    if ruleset < 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    // SAFETY: the kernel just returned this descriptor and nothing else owns it.
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset) };
    for path in writable {
        let parent = rustix::fs::open(
            *path,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| ToolError::ProtectionUnavailable)?;
        let directory = rustix::fs::fstat(&parent)
            .map(|stat| rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir())
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        let rule = PathBeneathAttr {
            // A non-directory accepts only file rights.
            allowed_access: if directory {
                handled
            } else {
                handled & (WRITE_FILE | TRUNCATE)
            },
            parent_fd: parent.as_raw_fd(),
        };
        // SAFETY: `rule` is a live, initialized packed attribute matching the UAPI layout, and
        // both descriptors stay open for the duration of the call.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd() as libc::c_long,
                RULE_PATH_BENEATH,
                &raw const rule,
                0 as libc::c_long,
            )
        };
        if added != 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1) takes integer arguments only and affects this thread.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    // SAFETY: integer arguments only; it confines this thread and its future descendants.
    let restricted = unsafe {
        libc::syscall(
            libc::SYS_landlock_restrict_self,
            ruleset.as_raw_fd() as libc::c_long,
            0 as libc::c_long,
        )
    };
    if restricted != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    super::profile::install_metadata_filter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_confined_child_writes_only_beneath_its_grant_while_the_guard_keeps_access() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(workspace.join("kept.txt"), "kept\n").unwrap();
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(workspace.join("kept.txt"))
            .unwrap()
            .mode()
            & 0o777;
        let script = format!(
            "printf x > {scratch}/allowed; printf x > {workspace}/denied; printf x > {workspace}/.git/denied; printf y > {workspace}/kept.txt; truncate -s 0 {workspace}/kept.txt; chmod 0777 {workspace}/kept.txt; chmod 0777 {scratch}/allowed; touch -d @1 {workspace}/kept.txt; ln {workspace}/kept.txt {scratch}/link; rm -f {workspace}/kept.txt; mkdir {workspace}/dir; printf x > /dev/null; cat {workspace}/kept.txt",
            scratch = scratch.display(),
            workspace = workspace.display(),
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let output = spawn_confined(&mut command, &[&scratch, Path::new("/dev/null")])
            .expect("Landlock is required for typed Git writes")
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(output.stdout, b"kept\n", "reads stay available");
        assert!(scratch.join("allowed").exists());
        for denied in ["denied", ".git/denied", "dir"] {
            assert!(!workspace.join(denied).exists(), "{denied}");
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("kept.txt")).unwrap(),
            "kept\n"
        );
        let kept = std::fs::metadata(workspace.join("kept.txt")).unwrap();
        assert_eq!(kept.mode() & 0o777, mode, "mode unchanged");
        assert_ne!(kept.mtime(), 1, "time unchanged");
        assert_eq!(kept.nlink(), 1, "no link was made into scratch");
        // Metadata changes are refused even in the writable view.
        let allowed = std::fs::metadata(scratch.join("allowed")).unwrap();
        assert_ne!(allowed.mode() & 0o777, 0o777);
        // Inode-flag and similar ioctls work through a read-only descriptor, so every ioctl fails.
        let file = std::fs::File::open(workspace.join("kept.txt")).unwrap();
        let confined = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    restrict_thread(&[&scratch]).unwrap();
                    rustix::fs::ioctl_getflags(&file)
                })
                .join()
                .unwrap()
        });
        assert_eq!(confined.err(), Some(rustix::io::Errno::NOTTY));
        std::fs::write(workspace.join("guard.txt"), "guard").unwrap();
    }
}

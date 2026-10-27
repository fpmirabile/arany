use crate::tools::ToolError;
use crate::validate::digest;
use nix::libc;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Namespaces {
    mnt: u64,
    net: u64,
    pid: u64,
    ipc: u64,
    uts: u64,
    user: u64,
}

impl Namespaces {
    pub(super) fn supervisor_isolated_from(&self, parent: &Self) -> bool {
        self.mnt != parent.mnt && self.user != parent.user
    }
    pub(super) fn read() -> Result<Self, ToolError> {
        let inode = |name| {
            std::fs::metadata(format!("/proc/self/ns/{name}"))
                .map(|metadata| metadata.ino())
                .map_err(|_| ToolError::ProtectionUnavailable)
        };
        Ok(Self {
            mnt: inode("mnt")?,
            net: inode("net")?,
            pid: inode("pid")?,
            ipc: inode("ipc")?,
            uts: inode("uts")?,
            user: inode("user")?,
        })
    }

    pub(super) fn isolated_from(&self, parent: &Self) -> bool {
        self.mnt != parent.mnt
            && self.net != parent.net
            && self.pid != parent.pid
            && self.ipc != parent.ipc
            && self.uts != parent.uts
            && self.user != parent.user
    }
}

fn compile(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    action: SeccompAction,
) -> Result<BpfProgram, ToolError> {
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        action,
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| ToolError::ProtectionUnavailable)?,
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?
    .try_into()
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    #[cfg(target_arch = "x86_64")]
    let program = {
        use seccompiler::sock_filter;
        let mut prefix = vec![
            sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: 0x45,
                jt: 0,
                jf: 1,
                k: 0x4000_0000,
            },
            sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_KILL_PROCESS,
            },
        ];
        prefix.extend(program);
        prefix
    };
    if program.is_empty() || program.len() > 1024 {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(program)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Routing {
    Offline,
    Proxy,
}

const AF_INET: u64 = libc::AF_INET as u64;
const AF_INET6: u64 = libc::AF_INET6 as u64;

pub(super) fn programs(routing: Routing) -> Result<[BpfProgram; 2], ToolError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let clone3 = compile(
            [(libc::SYS_clone3, Vec::new())].into(),
            SeccompAction::Errno(libc::ENOSYS as u32),
        )?;
        let mut rules = BTreeMap::new();
        for syscall in [
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_sendmsg,
            libc::SYS_sendmmsg,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_pidfd_getfd,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_keyctl,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_kexec_load,
            libc::SYS_kexec_file_load,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_mount_setattr,
            libc::SYS_open_by_handle_at,
            libc::SYS_userfaultfd,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            rules.insert(syscall, Vec::new());
        }
        if routing == Routing::Proxy {
            // The private namespace holds only loopback and the bridge; named sockets stay unreachable.
            rules.remove(&libc::SYS_connect);
            let outside = |family| {
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, family)
                    .map_err(|_| ToolError::ProtectionUnavailable)
            };
            // A datagram pair can reconnect to a visible named socket; stream and seqpacket pairs are already connected.
            let datagram = SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(0xf),
                libc::SOCK_DGRAM as u64,
            )
            .map_err(|_| ToolError::ProtectionUnavailable)?;
            rules.insert(
                libc::SYS_socketpair,
                vec![
                    SeccompRule::new(vec![datagram])
                        .map_err(|_| ToolError::ProtectionUnavailable)?,
                ],
            );
            rules.insert(
                libc::SYS_socket,
                vec![
                    SeccompRule::new(vec![outside(AF_INET)?, outside(AF_INET6)?])
                        .map_err(|_| ToolError::ProtectionUnavailable)?,
                ],
            );
        }
        let destination = SeccompCondition::new(4, SeccompCmpArgLen::Qword, SeccompCmpOp::Ne, 0)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        rules.insert(
            libc::SYS_sendto,
            vec![
                SeccompRule::new(vec![destination])
                    .map_err(|_| ToolError::ProtectionUnavailable)?,
            ],
        );
        #[cfg(target_arch = "x86_64")]
        for syscall in 512..548 {
            rules.insert(syscall, Vec::new());
        }
        let mut namespace_rules = Vec::new();
        for flag in [
            libc::CLONE_NEWNS,
            libc::CLONE_NEWCGROUP,
            libc::CLONE_NEWUTS,
            libc::CLONE_NEWIPC,
            libc::CLONE_NEWUSER,
            libc::CLONE_NEWPID,
            libc::CLONE_NEWNET,
        ] {
            let condition = SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(flag as u64),
                flag as u64,
            )
            .map_err(|_| ToolError::ProtectionUnavailable)?;
            namespace_rules.push(
                SeccompRule::new(vec![condition]).map_err(|_| ToolError::ProtectionUnavailable)?,
            );
        }
        rules.insert(libc::SYS_clone, namespace_rules);
        Ok([
            clone3,
            compile(rules, SeccompAction::Errno(libc::EPERM as u32))?,
        ])
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn profile_digest(routing: Routing) -> Result<[u8; 32], ToolError> {
    let mut bytes = Vec::new();
    for program in programs(routing)?.into_iter().chain([
        non_spawning_program()?,
        metadata_program()?,
        ioctl_program()?,
        unknown_syscalls_program()?,
    ]) {
        bytes.extend_from_slice(&(program.len() as u32).to_le_bytes());
        for instruction in program {
            bytes.extend_from_slice(&instruction.code.to_le_bytes());
            bytes.push(instruction.jt);
            bytes.push(instruction.jf);
            bytes.extend_from_slice(&instruction.k.to_le_bytes());
        }
    }
    Ok(digest(&bytes))
}

pub(super) fn install(routing: Routing) -> Result<(), ToolError> {
    for program in programs(routing)? {
        // TSYNC filters every thread; per-thread filters would leave others unfiltered.
        seccompiler::apply_filter_all_threads(&program)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    Ok(())
}

fn non_spawning_program() -> Result<BpfProgram, ToolError> {
    let mut rules = BTreeMap::new();
    for call in [
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_execveat,
    ] {
        rules.insert(call, Vec::new());
    }
    #[cfg(target_arch = "x86_64")]
    for call in [libc::SYS_fork, libc::SYS_vfork] {
        rules.insert(call, Vec::new());
    }
    compile(rules, SeccompAction::Errno(libc::EPERM as u32))
}

// Landlock does not mediate these metadata changes, and a Git write's child needs none of them.
fn metadata_program() -> Result<BpfProgram, ToolError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // Generic syscall numbers, equal on both architectures, that libc does not name everywhere.
        const FCHMODAT2: i64 = 452;
        const SETXATTRAT: i64 = 463;
        const REMOVEXATTRAT: i64 = 466;
        const FILE_SETATTR: i64 = 469;
        let mut rules = BTreeMap::new();
        for call in [
            libc::SYS_fchmod,
            libc::SYS_fchmodat,
            FCHMODAT2,
            libc::SYS_fchown,
            libc::SYS_fchownat,
            libc::SYS_utimensat,
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_fsetxattr,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_fremovexattr,
            SETXATTRAT,
            REMOVEXATTRAT,
            FILE_SETATTR,
        ] {
            rules.insert(call, Vec::new());
        }
        #[cfg(target_arch = "x86_64")]
        for call in [
            libc::SYS_chmod,
            libc::SYS_chown,
            libc::SYS_lchown,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_futimesat,
        ] {
            rules.insert(call, Vec::new());
        }
        compile(rules, SeccompAction::Errno(libc::EPERM as u32))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

// Any later syscall may be a new metadata setter: ENOSYS fails closed and reads as an old kernel.
const LAST_KNOWN_SYSCALL: u32 = 469;

// A range check too wide for per-number rules; architecture refusals stay with the Guard filters.
fn unknown_syscalls_program() -> Result<BpfProgram, ToolError> {
    above_program(LAST_KNOWN_SYSCALL)
}

fn above_program(last: u32) -> Result<BpfProgram, ToolError> {
    use seccompiler::sock_filter;
    const LOAD_NR: u16 = 0x20;
    const JUMP_ABOVE: u16 = 0x25;
    const RETURN: u16 = 0x06;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    let instruction = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    Ok(vec![
        instruction(LOAD_NR, 0, 0, 0),
        instruction(JUMP_ABOVE, 0, 1, last),
        instruction(RETURN, 0, 0, RET_ERRNO | libc::ENOSYS as u32),
        instruction(RETURN, 0, 0, RET_ALLOW),
    ])
}

// Inode-flag, fsxattr and version ioctls bypass read-only descriptors; ENOTTY keeps isatty honest.
fn ioctl_program() -> Result<BpfProgram, ToolError> {
    compile(
        [(libc::SYS_ioctl, Vec::new())].into(),
        SeccompAction::Errno(libc::ENOTTY as u32),
    )
}

// Calling thread only: the Git child inherits them while the Guard thread keeps its access.
pub(super) fn install_metadata_filter() -> Result<(), ToolError> {
    for program in [
        metadata_program()?,
        ioctl_program()?,
        unknown_syscalls_program()?,
    ] {
        seccompiler::apply_filter(&program).map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn install_above(last: u32) -> Result<(), ToolError> {
    seccompiler::apply_filter(&above_program(last)?).map_err(|_| ToolError::ProtectionUnavailable)
}

pub(super) fn install_non_spawning() -> Result<(), ToolError> {
    seccompiler::apply_filter_all_threads(&non_spawning_program()?)
        .map_err(|_| ToolError::ProtectionUnavailable)
}

use crate::tools::{ToolError, types::digest};
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
    let mut program: BpfProgram = SeccompFilter::new(
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
    {
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
        prefix.append(&mut program);
        program = prefix;
    }
    if program.is_empty() || program.len() > 1024 {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(program)
}

pub(super) fn programs() -> Result<[BpfProgram; 2], ToolError> {
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

pub(super) fn profile_digest() -> Result<[u8; 32], ToolError> {
    let mut bytes = Vec::new();
    for program in programs()? {
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

pub(super) fn install() -> Result<(), ToolError> {
    for program in programs()? {
        seccompiler::apply_filter(&program).map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    Ok(())
}

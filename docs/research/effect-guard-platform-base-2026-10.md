# Effect Guard platform evidence

Source baseline: 2026-10-04. Primary-source review and read-only Linux host observations; no payload, filter, service or native macOS test was executed by this investigation.

[Tool behavior](../specs/tools.md), [Guard rules](../../agents/guard.md) and [local Tool evidence](../tools.md) own the implementation, platform claims and outstanding verification. [Protection research](./deterministic-harness-protection.md) retains the broader threat model. This report preserves the source distinctions behind the platform design, not a second launch protocol or implementation checklist.

## Source facts that determine the design

- Bubblewrap constructs a namespace environment; its policy is the caller's responsibility. Its empty mount view can expose only selected read-only/runtime and writable/staging objects. Mounting a D-Bus socket can recover host execution authority, and terminal access needs care. [Bubblewrap official README](https://github.com/containers/bubblewrap/blob/v0.13.0/README.md).
- Bubblewrap 0.13.0 accepts opened-object mounts, seccomp programs, explicit namespace setup and a setup barrier. Its implementation closes surplus descriptors, drops setup privileges and verifies disabled nested user namespaces. These facilities need a compiled policy and a trusted bootstrap; a version string or info record alone is not a complete attestation. [Reviewed version's source](https://github.com/containers/bubblewrap/blob/v0.13.0/bubblewrap.c).
- `openat2` supports root-relative lookup without symlinks, magic links or mount crossings; lookup races can reject rather than resolve an escape. [Linux `openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html).
- Hard links alias one inode. A writable live-Workspace bind can therefore change an outside-linked inode even though no outside pathname is visible. Replacing a directory entry with a new inode does not mutate other links to the replaced inode. [Linux `link(2)`](https://man7.org/linux/man-pages/man2/link.2.html), [Linux `rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html).
- Network namespaces isolate protocol stacks and abstract Unix sockets; omitting host pathname sockets and inherited sockets remains separately necessary. [Linux network namespaces](https://man7.org/linux/man-pages/man7/network_namespaces.7.html).
- seccomp is syscall filtering, not a complete sandbox; it cannot dereference argument pointers. Descendants inherit filters. [Kernel seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html).
- Landlock restrictions compose monotonically and inherit to new descendants. Available rights depend on the runtime ABI; already-open file/socket capabilities need independent handling. TCP/UDP port rules do not identify an HTTPS origin or permitted payload. [Kernel Landlock documentation, August 2026](https://docs.kernel.org/userspace-api/landlock.html).
- cgroup v2 provides hierarchical process and memory limits, CPU bandwidth controls, and whole-tree `cgroup.kill`. Delegation and actual controller availability matter. `cpu.max` is bandwidth, not a total CPU-time budget. [Kernel cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html).
- Ordinary resource limits are largely per-process; `RLIMIT_NPROC` counts a real UID's tasks and has privilege exceptions. They supplement, not replace, aggregate Run containment. [Linux `getrlimit(2)`](https://man7.org/linux/man-pages/man2/getrlimit.2.html).
- Namespace init death terminates the namespace's processes. This is a cleanup primitive, not aggregate resource admission. Killing a command PID or process group does not by itself demonstrate whole-tree cleanup. [Linux PID namespaces](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html).

## Filesystem and process-boundary trade-offs

A broker for closed, typed file operations has a smaller trusted surface than an arbitrary-code launcher. A separate process alone does not provide arbitrary-code containment. Likewise, Landlock restrictions alone do not establish filesystem visibility, descriptor hygiene or aggregate descendant-resource ownership; these are independent properties of a complete launch boundary.

The hard-link behavior above explains private fresh-inode staging instead of writable live-Workspace binds for untrusted commands. Replacing a directory entry differs from mutating its shared inode. A pinned directory handle identifies an object, not an eternal pathname: another host process can move the directory. These distinctions explain the scoped threat model and revalidation needs without restating the current write protocol.

## Read-only Linux observations

On 2026-10-04 at 07:36 UTC, the inspected host had kernel `7.2.7-zen1-1-zen`, active Landlock, root-owned non-setuid Bubblewrap 0.13.0, systemd 262 and cgroup v2. The current user's service reported delegation of `cpu memory pids`; some application/terminal descendant controls were writable. No cgroup changes, namespace payload, service startup, keyring access or secret inspection occurred.

These observations showed no demonstrated install/root blocker for further investigation on that host. They did not establish Arany ownership of shared terminal groups, usable Landlock rights, successful isolation or portability to another Linux installation. Systemd's single-writer delegation contract explains why writable manager-owned controls are not automatically a capability available to an application. [Delegation contract](https://systemd.io/CGROUP_DELEGATION/).

## Service-manager lifecycle distinctions

The systemd 262 review found several distinctions relevant to an external cleanup owner:

- `systemd-run --pipe` forwards standard input/output/error, not arbitrary Engine file descriptors. Killing its client does not demonstrate that the transient service stopped or its pipes reached EOF. [Launcher contract](https://github.com/systemd/systemd/blob/v262/man/systemd-run.xml), [implementation](https://github.com/systemd/systemd/blob/v262/src/run/run.c).
- Requested service properties and effective kernel controls are different evidence. Manager-applied limits do not imply delegation; delegation is needed when the application manages a subtree itself. [Manager API](https://github.com/systemd/systemd/blob/v262/man/org.freedesktop.systemd1.xml), [delegation](https://systemd.io/CGROUP_DELEGATION/).
- Activation, runtime and shutdown have distinct lifetimes. A stop acknowledgement and a main-process exit do not establish empty descendant ownership. Early unit collection can also discard observation state; output EOF can occur while malicious descendants remain alive. [Service lifecycle](https://github.com/systemd/systemd/blob/v262/man/systemd.service.xml), [group termination](https://github.com/systemd/systemd/blob/v262/man/systemd.kill.xml), [control CLI](https://github.com/systemd/systemd/blob/v262/man/systemctl.xml).

Those facts explain independent invocation identity, effective-capability evidence and complete-tree cleanup ownership. They do not establish that the original candidate service had passed a native journey.

## Seccomp source findings

Bubblewrap 0.13.0 set no-new-privileges and failed on setup error. A launcher argument or pre-existing `Seccomp=2` was not proof that Arany's intended filter had been installed. [Pinned setup](https://github.com/containers/bubblewrap/blob/v0.13.0/bubblewrap.c#L2735-L2737).

The published seccompiler 0.5.0 safe installer hid internal `prctl`/seccomp FFI and converted program length to `u16`. Its reviewed target support was little-endian x86-64, AArch64 and RISC-V64. Avoiding application unsafe therefore did not remove filter-size, target or dependency-review concerns. The standalone upstream had moved development to the Rust-VMM monorepo; this report's findings are pinned to the published source, not a maintained-fork recommendation. [Installer](https://github.com/rust-vmm/seccompiler/blob/v0.5.0/src/lib.rs#L294-L360), [published target support](https://docs.rs/seccompiler/0.5.0/seccompiler/), [upstream move](https://github.com/rust-vmm/seccompiler).

Two inspected ABI details explained compatibility and escape risks:

- Classic seccomp cannot inspect the flags behind `clone3`'s pointer argument. Glibc 2.42's internal wrapper fell back to legacy clone on `ENOSYS`, not arbitrary `EPERM`. Filter outcomes could therefore break legitimate threading/build descendants even when both errors appeared to deny the same call. [Namespace-flags UAPI](https://github.com/torvalds/linux/blob/v6.17/include/uapi/linux/sched.h), [Glibc fallback](https://github.com/bminor/glibc/blob/glibc-2.42/sysdeps/unix/sysv/linux/clone-internal.c).
- The inspected compiler checked audit architecture but did not reject the x32 syscall bit. An architecture check alone was therefore insufficient for the intended x86-64 profile. [Pinned BPF generator](https://github.com/rust-vmm/seccompiler/blob/v0.5.0/src/backend/bpf.rs), [kernel ABI warning](https://man7.org/linux/man-pages/man2/seccomp.2.html).

A default-allow filter reducing selected escape primitives remains defense in depth, not a complete syscall allowlist or a guarantee about future kernel APIs. Filesystem, network, descriptors and aggregate resources remain independent enforcement dimensions.

Denying the `ptrace` syscall alone does not deny `/proc/<pid>/mem` access, which uses a ptrace permission check internally. This distinction motivated the trusted supervisor's dumpability boundary; it did not expand the threat model to arbitrary hostile host/root processes. [Rustix dumpability API](https://github.com/bytecodealliance/rustix/blob/v1.1.5/src/process/prctl.rs#L113-L131), [proc memory access](https://man7.org/linux/man-pages/man5/proc_pid_mem.5.html).

## Platform-evidence limit

Apple's entitlement-based App Sandbox documentation did not establish dynamic per-Run arbitrary-tool containment or Linux-equivalent resource/tree guarantees for Arany. That was a limit of this original comparison, not a current claim that the macOS adapter is absent. Current native evidence and gaps belong to the Tool owners linked above. [Apple App Sandbox](https://developer.apple.com/documentation/security/app-sandbox), [machine-readable documentation](https://developer.apple.com/tutorials/data/documentation/security/app-sandbox.json).

Installed utilities, source inspection and compilation do not establish successful native containment, cleanup or OS parity. No paid call, account mutation, package installation or native execution result is established by this report.

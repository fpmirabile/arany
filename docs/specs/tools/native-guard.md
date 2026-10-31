# Tools: native Guard enforcement

## macOS enforcement and compatibility

Primary Workspace mutations and typed discovery use no Guard unit. Skill loading and definition parsing use a non-spawning profile that never grants Workspace writes. Commands and MCP run native binaries with ordinary child launches, `posix_spawn` included.

Commands and MCP run in a separate attested Seatbelt worker under a trusted, bounded supervisor in its own kernel resource coalition, launched in the Background launchd user domain; there is no unsandboxed fallback. Before Tools are offered, admission probes that domain and coalition support (three-second deadline); availability is not attestation, and unit authentication, coalition ownership, storage preparation and installed enforcement are checked again before GO. A stale executable pin rejects before any Provider request and keeps the committed history prefix. Completion is accepted only after every member of the unit, descendants included, has terminated and its storage holds no mount; a process group or PID never counts as containment. Without the Apple compiler and SDK profiles, a guarded command cannot read or execute the Command Line Tools or an SDK, even where the host baseline allows it.

Deadlines and native-manager failures name their stage without native diagnostics. Teardown shares one ten-second bound; a failed teardown names a closed Guard stage (process cleanup, launcher reap, output EOF or storage cleanup), blocks acceptance, and no worker text reaches the model; other post-dispatch failures report the execution stage.

Default and Build profiles kill the whole unit when a threshold is exceeded or observation fails, and a missing observation fails closed. Memory (aggregate physical footprint) and process/thread thresholds are supervised, not kernel ceilings: work between samples can exceed them, and no swap prohibition or group OOM is claimed. CPU is limited per process, so tree CPU and instantaneous rate are unbounded; Build's threshold follows its wall timeout, the default keeps 15 seconds per process. Wall time, output, the disk reserve, offline isolation and complete descendant termination are always enforced.

Each unit's `workspace`, `scratch` and cache view are owner-only directories on the host volume; writes are confined to them, and a hard-link alias of a readable host file cannot be written. Copy and capture caps, the changeset ceiling and the cache candidate cap bound what is read and published. Disk use is bounded by a [2 GiB](../limits.md#guard_disk_reserve_bytes) free-space reserve on that volume, the margin the journal needs, counting other writers; there is no per-command growth cap. Below the reserve a unit is refused before launch with `Limit(resource)` naming the free and reserved MiB; a running command is stopped at the reserve with a fixed `[arany] Command stopped` line appended to stderr, keeping its output and publishing nothing; any unit is killed at half the reserve. Linux payload writes land in memory-bounded tmpfs, so there the reserve concerns only the build cache.

Unit storage and processes left by a crashed controller are recovered at the next admission: at most eight roots owned by the user within one 30-second<!-- unrelated-bound: macOS native recovery deadline --> deadline, only with valid records and matching identity, never by name prefix; after a reboot nothing is signaled. Roots without valid records stay untouched and count toward capacity. Unreadable or over-limit listings and deadline expiry reject as unavailable protection. Recovery never retries an old intent.

Commands and MCP get a fresh checked Workspace snapshot, prior typed edits included, and a read-only tree of pinned runtime resources; writable storage prohibits set-ID. System executables must be immutable, root-owned, non-set-ID and unentitled, and the executable catalogue is checked once per Run, bound into each intent and receipt, and revalidated before Ready; changes reject without retry. Granted `PATH` directories, system link targets, Homebrew prefixes and manager roots are readable and executable without those checks. Privileged process, credential and coalition changes, account and Session roots, network and Mach services stay denied; root or mismatched real and effective identities reject. Metadata is visible only for required roots and their exact ancestors, never as listings; `/` exposes only its top-level names; `/private/etc/ssl` (TLS configuration and CA bundle) is readable, because system TLS clients exit without it, and no other `/private/etc` path is. Descendant mounts under selected roots reject before launch, a point-in-time check.

macOS uses private physical paths, not a mount namespace; the working directory is in the project snapshot. Standalone argv paths under `/workspace`, `/scratch`, `/cache`, `/inputs`, `/skills` or `/programs` map into private storage, and copied-program aliases apply to copied executables. Shell strings are not rewritten; scripts use `ARANY_TOOL_WORKSPACE`, `ARANY_TOOL_INPUTS`, `ARANY_TOOL_SKILLS` or relative paths. On both platforms the pinned `/bin/sh` accepts only `-c` followed by a script not beginning with `-` or `+`; anything else is refused before the shell starts. Arany never starts a login shell or sources profiles; a script may source a file it names. `HOME` and `TMPDIR` point to scratch. System binaries run at their original paths.

The environment is cleared; `SDKROOT` is set only for the exact Apple SDK profile. sysctl, host CPU discovery, named socket creation, network operations and descriptor-passing messages are denied, attested before GO so no payload runs under a partial profile. A pinned compatibility library answers only page-size queries (`sysconf(_SC_PAGESIZE)`, `getpagesize()`); a configured Prefix wrapper must evaluate the script through a shell for it to apply, and nested SIP-protected shells get no page-size compatibility claim. Selected-Rust Build commands get the Goal-pinned jobs default.

## Shared native capacity and settings

Linux and macOS share Guard capacity across processes and Session roots through the effective user's account root (`~/Library/Application Support/dev.Arany.arany` on macOS, `~/.local/state/arany` on Linux), not Session storage. The owner-only file `guard/limits.json` is created on first native admission with:

```json
{"units":8,"queued":32}
```

Edit it to change capacity, keeping mode `0600` and the private directory. `units` accepts 1–64 and `queued` 1–1024; unknown fields, unsafe files and invalid ranges reject. Every admission rereads it. Lowering a limit kills no admitted unit or waiter; no new admission exceeds it.

Every Guard unit, file-only included, reserves one slot before launch; live, recovering and retained units count, and excess roots from an earlier implementation are kept and block admission. macOS unit storage is bounded by the [disk reserve](#macos-enforcement-and-compatibility). A Linux slot owns its host seed of at most [128 MiB](../limits.md#command_snapshot_bytes) of Workspace data and [96 MiB](../limits.md#max_runtime_bytes) of runtime data until checked deletion, so new retained payload is at most `units × 224 MiB` (1.75 GiB by default): a logical bound, not a filesystem quota.

When capacity is full, the approved Tool stays pending; each Run has at most one. Up to `queued` Tools wait in one shared FIFO; newcomers cannot overtake. Cancellation, the capacity deadline and process death remove a waiter. The deadline starts at post-approval queue entry, is bounded by the pinned runtime allowance and is a capacity refusal, not a human rejection. Preparation and GO then have their own deadlines, so a short command timeout is not refused because preparation outlasts it. Overflow rejects before effects. Waiting renews no intent, approval or budget, is not durable, and restart resumes no permission. The [terminal contract](../terminal/conversation.md#conversation-progress-and-history) owns the queued indication.

### Linux host seed ownership

A host seed keeps its capacity reservation until checked deletion; normal completion leaves neither. After controller death, the next admission recovers owned storage before releasing capacity, without replaying the effect. Failed deletion, identity uncertainty or an ambiguous launch keeps the charge, which can hold capacity until native evidence or a later boot allows recovery. Deletion refuses links, mounts, foreign ownership, unsafe permissions and replacements, and normal admission never migrates or deletes unsupported records. Unrecorded historical roots need the [one-time maintenance procedure](../../tools.md#historical-linux-seed-maintenance); a name prefix never authorizes removal.

## Linux enforcement and compatibility

Primary Workspace mutations, typed discovery and typed Git use no Guard unit or Workspace mount. [Command input selection](./changesets-and-checkpoints.md#command-input-selection) covers overlay and copy views.

The enforcer requires non-set-ID, root-owned `/usr/bin/bwrap`, `/usr/bin/systemd-run` (supporting `--expand-environment=no`), `/usr/bin/systemctl` and `/usr/bin/env`, unprivileged user namespaces, a systemd user manager, cgroup v2 with effective memory, swap, PID and CPU limits and owned termination, and tmpfs user xattrs (Linux 6.6 or a backport, verified by operation). Other init systems need their own enforcing adapter; unsupported architectures or missing enforcement fail closed. No desktop environment is required. A selected toolchain whose host path overlaps a fixed Guard mount target rejects with Configuration before setup.

The supervisor executable must be a regular file without set-ID or group/other write bits (a group-writable umask build may need `chmod go-w target/debug/arany`); unsafe modes fail as `ChangedInput` even with unchanged bytes. Only the pinned bytes run, so replacing the path cannot select new code; unsupported checks fail closed without repinning or path fallback. The AppArmor prerequisite below stays required, with no wildcard or proc-path profile.

Missing namespace authorization reports `NamespaceEntry`, and failed mounts, topology or xattrs `RuntimeMounts`, each `ProtectionUnavailable` before GO. Primary commands see admitted inputs as read-only layers under a private, bounded in-memory writable layer, and exact files read-only; any cloning, overlay or topology failure refuses with no copy or host fallback. A granted root containing any host submount (a tmpfs `target/`, a bind-mounted vendor tree, a nested devcontainer volume) refuses as `ProtectionUnavailable` before setup; narrowing the grant avoids it, and a root that is itself a mount is a different case. Granted `PATH` directories, system link targets, Homebrew prefixes and manager roots appear read-only and executable at their host paths. Mount topology is attested before Ready and after GO, and later host mounts cannot propagate in.

Before releasing a one-use intent the Guard verifies isolated mount, network, PID, user, IPC and UTS namespaces, dropped capabilities, no-new-privileges, inherited syscall filters and effective limits. It owns the unit's cgroup, double-fork and setsid descendants included, and verifies termination before accepting completion. Helper or report death after GO is [Uncertain](../execution/events-and-outcomes.md#effect-outcomes) and publishes nothing; capture yields a bound candidate or a known refusal, anything else is Uncertain. The seccomp deny profile is defense in depth, not an allowlist. No ARM64 support is claimed.

Snapshot byte and entry bounds do not guarantee a finite response from stalled storage or an uninterruptible kernel call; deadlines bound ordinary payloads, not kernel failures. File preconditions are not a multi-file transaction and do not exclude hostile host mutation.

### Ubuntu namespace-entry diagnosis

Creating a user namespace does not grant permission to map IDs or mount in it. On Ubuntu 24.04 [AppArmor lets unconfined executables create user namespaces but denies capabilities inside them](https://documentation.ubuntu.com/release-notes/24.04/#unprivileged-user-namespace-restrictions), giving `ProtectionUnavailable` even with the kernel facilities present; compare the stage, profile labels and capability sets before blaming AppArmor. Ubuntu 24.04 ships `bwrap-userns-restrict` as an [extra profile](https://gitlab.com/apparmor/apparmor/-/raw/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict), enabled by default since [25.04](https://documentation.ubuntu.com/release-notes/25.04/#apparmor-profile-for-bwrap); with it, Arany also needs its pinned supervisor authorized. [CI](../../../.github/workflows/ci.yml) disables the restriction for Linux native lanes, so they do not prove stock-policy compatibility.

When the namespace is denied, guarded commands fail closed as `ProtectionUnavailable` before GO with this diagnosis and the remedy below, showing the admitted supervisor path as an escaped literal, never a model- or repository-supplied path or a wildcard.

**Preferred one-time administrator step.** Add executable-specific `userns` exceptions for `/usr/bin/bwrap` and the exact installed supervisor path, replacing the example `/opt/arany/bin/arany`; never a symlink, version wildcard, directory or terminal/shell profile. If `aa-status` already lists a profile attached to `/usr/bin/bwrap`, omit `arany_bwrap`, because [AppArmor denies exec when it cannot choose between attachments](https://manpages.ubuntu.com/manpages/noble/man5/apparmor.d.5.html). The administrator runs these by hand; Arany never makes policy changes or executes privilege tools, even with a host-command approval.

```sh
sudo tee /etc/apparmor.d/arany-userns >/dev/null <<'PROFILE'
abi <abi/4.0>,

profile arany_bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
}

profile arany_supervisor /opt/arany/bin/arany flags=(unconfined) {
  userns,
}
PROFILE
sudo chmod 0644 /etc/apparmor.d/arany-userns
sudo apparmor_parser -r /etc/apparmor.d/arany-userns
```

Loading applies without a reboot and persists in `/etc/apparmor.d`. Keep `kernel.apparmor_restrict_unprivileged_userns=1`, restart Arany from the profiled executable and retry. The profile alone does not establish mount, xattr, overlay or full Guard support. These `flags=(unconfined)` profiles only authorize namespaces for the named executables; Arany still enforces zero payload capabilities, no-new-privileges, seccomp and its grants, and they authorize no host command or wider access. [Attachments](https://manpages.ubuntu.com/manpages/noble/man5/apparmor.d.5.html) match paths, not digests: a moved executable (`cargo install`, upgrade, relocation) fails closed until the administrator updates the path, and a replacement at the same path keeps the exception, so trust what is installed there. Do not broaden the attachment to future versions.

**Last resort.** If targeted profiles are unavailable or insufficient, an administrator may by hand choose this temporary system-wide change:

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

It lifts the restriction for every unconfined application, widening their kernel attack surface, supplies no other missing enforcement and is not profile-enabled evidence. Restore it with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1`; it normally resets at reboot unless persisted.

To diagnose from the checkout, in a disposable unprivileged account on stock Ubuntu 24.04, run `cargo run --locked --offline -p arany --example namespace_entry_probe`. The [probe](../../../crates/arany/examples/namespace_entry/probe.rs) runs no command payload; it records kernel, package and profile provenance, labels, UID/GID maps and capability sets, and on success a bounded tmpfs round trip, capability removal and no-new-privileges across exec. It is not covered by the Arany attachment, so neither it nor a sysctl-disabled run shows that the profiled supervisor works under the stock restriction.

# Native Guard

Load when changing `src/tools/guard.rs` or its native adapters. [Tools](./tools.md)
owns Policy and effects; [the Tool spec](../docs/specs/tools.md) owns observable
limits and compatibility.

- Keep native enforcement private to the bounded same-binary helpers. A macOS
  [unit supervisor](./guard-unit.md) owns the coalition and a separate Seatbelt
  worker owns payload enforcement. The macOS
  adapter admits typed files and pinned Skill data through a non-spawning
  profile. Commands/MCP use the private snapshot and ordinary-launch profile;
  keep those authorities distinct and reject unavailable enforcement.
- Close inherited descriptors at the single-threaded worker entry before Rust
  opens persistent handles. Install shared descriptor/core limits at bootstrap;
  only the non-spawning file/Skill worker resets and unblocks SIGXCPU and installs
  hard CPU expiry before Seatbelt prohibits changes. Commands/MCP and the
  supervisor use external CPU observation. Attest denial before GO; no payload
  may run under a partially installed profile.
  The trusted unit supervisor closes inherited descriptors at its own entry too.
- Keep process-environment sysctl access denied. Named socket creation, network
  operations and descriptor-bearing messages are prohibited, including native
  delegated/non-cancellable syscall variants. Anonymous pair creation does not
  imply its socket writes remain available under Seatbelt.
- Retain the exact owned child until reap. Stop PID observation immediately
  after reap. Darwin task information disappears during exit; BSD inspection
  must include zombies (`arg = 1`) and positively establish kernel exit before
  retiring resource observation. Missing live-process information fails closed.
  Inside the macOS worker, poll child exit within the existing deadline;
  sandboxed execution must not depend on Tokio's signal wake socket.
- Guard's native adapter and the separate [private filesystem ACL adapter](./private-fs.md)
  are the isolated unsafe Rust exceptions. Match the installed SDK and libc
  ABI, validate exact initialized output sizes, bound descriptor/profile buffers,
  retain C string lifetimes through each call and free native error allocations
  once. Never parse or print arbitrary native diagnostics. The syscall-number
  filter and explicit Darwin syscall numbers support only reviewed 64-bit ABIs.
- Keep Linux runtime aliases distinct: `/lib` targets `/usr/lib`, while `/lib64`
  targets `/usr/lib64`. Mapping both to `/usr/lib` hides the x86-64 ELF loader
  even when the ARM64 native gates pass. Native fixtures must preserve the same
  loader layout as the production mount profile.
- Grant ancestor metadata only, without directory-data access; unrelated host
  metadata and control-root listings must remain denied. The compiled policy
  fingerprint covers profile rules, admission algorithms and enforced constants.
  For command/MCP grants, Tool admission checks one bounded system executable
  catalogue per Run; process intents bind its contents and file metadata stamps
  into the enforcement digest. Pass that typed catalogue only through the trusted
  controller/supervisor Task channel, never Workspace or payload input, and never
  persist it as an authority cache. Workers revalidate every entry through
  no-follow handles: root ownership, non-writable non-set-ID permissions,
  device/inode, size and modification/change timestamps. Reject drift before
  Ready. Pin the checked `/bin` and `/usr/bin` directory handles once per worker
  revalidation and open each file relative to its matching handle; never replace
  fresh file checks with cached metadata or reopen every ancestor for each entry.
  Encode the catalogue as one allow rule with an explicit union of its exact
  literal paths; retain all independent denies and the profile byte cap.
  Do not repeat the signature scan or refresh a stale catalogue within an
  admitted Run. Across Runs, a process may reuse an in-memory signature verdict
  only for the same path and an identical fresh metadata stamp; any drift
  reinspects the code. Never persist verdicts. Keep the catalogue inside the existing Task byte/entry bounds;
  overflow rejects without dispatch. File/Skill intents retain the compiled
  digest and carry no catalogue. Configured pins and selected resources remain
  bound by the normalized intent and configuration.
  The command profile additionally reads the literal filesystem root for the
  native loader; this does not grant reads beneath that directory. Do not
  remove it without native launch evidence: the loader opens `/` and aborts
  when denied, even with an admitted project working directory.
  Native shell selection also needs exact metadata for the OS shell selector
  and its path aliases; keep those literal runtime entries separate from grants.
  Return the admitted command root from profile installation and pass it into
  payload/MCP setup. Do not rediscover it with getcwd after Seatbelt: that can
  require directory-data access to the intentionally hidden control ancestors.
- Validate launchctl, env and hdiutil through the same no-follow OS-program
  admission before launching them. Path-based execution relies on their
  root-owned, non-writable system directories and files; privileged OS mutation
  is outside this current-user attacker boundary.
  Before offering Tools, also check kernel coalition observation and the effective
  user's launchd domain through a bounded data-free probe. Keep this admission
  probe separate from the policy fingerprint, which workers recompute under
  Seatbelt. Actual helper isolation and storage are still attested before GO.
- Native process profiles must deny credential and coalition changes through
  both direct syscalls and spawn attributes. Dynamically deny fork/spawn for
  actors holding coalition-spawn or persona-management authority after exec;
  source admission or `NOSUID` alone does not strip signed entitlements.
  Deny sandbox relabeling, clear inherited kernel bootstrap/registered ports
  before payload exec and attest ordinary unprivileged launch availability.
  Inspect only checked immutable OS-owned code through Security metadata;
  retain successful Copy results and release them with the Create rule.
- A dedicated FFI review covers allocation, alignment, initialization, ownership,
  variadic argument types, signal state and unreaped PID lifetime. Native profile
  conformance exercises denied host file/network/spawn access and immutable CPU
  controls in isolated child processes; the activated CPU owner proves actual
  expiry. The native Engine journey owns successful typed effects and closed
  replay. Passing either alone does not establish the other boundary.

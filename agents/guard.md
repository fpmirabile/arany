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
- Revalidate selected runtime path/version/directory identities before Ready
  and GO for process intents. File/Skill intents carry no runtime grants in
  their enforcement fingerprint or native profile and skip runtime rechecks.
  Linux mounts exact runtime and cache roots read-only; macOS grants each
  selected toolchain's whole subtree read and execute, so `bin` symlinks whose
  targets live elsewhere resolve, and read-only cache scopes. Do not grant parent
  cache/configuration paths. Candidate identity and consent rules live in
  [runtime profile rules](./toolchains.md).
- Linux `ro-bind` imports source submounts recursively. Start the unchanged
  Bubblewrap profile through the fixed, root-owned `/usr/bin/unshare` user and
  mount namespace bootstrap with recursive-private propagation, cleared
  environment and fixed arguments. Do not use `--keep-caps`: util-linux makes
  propagation private before exec, and retained capabilities make Bubblewrap
  reject a non-root real UID ([unshare](https://github.com/util-linux/util-linux/blob/v2.39.3/sys-utils/unshare.c),
  [Bubblewrap](https://github.com/containers/bubblewrap/blob/main/bubblewrap.c)).
  Do not add capabilities to payloads. Before
  Ready and again after GO, parse bounded `/proc/self/mountinfo` and require a
  private root and one exact private entry per selected runtime mount, with no optional
  propagation fields, stacked entries or descendant mounts; malformed or
  oversized data fails closed. The actual production fixture covers a shared
  source, a pre-existing nested mount refusal at the topology stage and a late
  host mount that does not appear in the isolated runtime mount. Its outer test owner
  uses a private PID namespace with `--kill-child`; a separate forced-timeout
  gate records the init and held descendant PIDs from each process's own
  `/proc/self/status`, binds both pidfds before killing and reaping the unshare
  owner, then awaits both exit signals before dropping the fixture directory.
  The topology path also gates the namespace init and each exact-filter worker
  until their host identities are bound, then confirms their pidfd exits after
  capture on success and failure. It keeps the fixture directory if any
  identity or exit remains unproved; assertions run only after confirmed exit.
  Publish PID records and release gates atomically so pollers observe either no
  signal or a complete record.
  The outer fixture keeps the host procfs view so those reports include host
  PIDs ([`NSpid` field](https://man7.org/linux/man-pages/man5/proc_pid_status.5.html));
  the production Bubblewrap profile still mounts its own `/proc`. Captured
  Rust helper output uses a full exact test filter and proves one test completed.
  This is separate from the systemd-owned full Guard journey. On macOS,
  canonicalize selected roots
  and reject strict descendant targets in the native `getfsstat` mount table
  before Seatbelt and before Ready/GO; this is a point-in-time check, not a
  lifetime mount-isolation guarantee.
- Capture command changesets only after positive payload-descendant absence.
  Linux retains its attested private PID namespace init/helper while stopping
  and killing every other namespace process; final owned-cgroup cleanup still
  gates completion. macOS retains the admitted snapshot baseline and checked
  storage handles through coalition cleanup, scans before detaching images,
  and never accepts a worker-local scan while descendants can still write.
  Both scans reject whole hostile/over-limit snapshots without reading excluded
  content. Engine owns command-receipt linkage; Guard independently validates
  the integration's whole batch and file preconditions.
- The persistent Workspace cache is imported into a separate quota-backed cache
  view before Ready/GO (Linux `/cache`; macOS cache volume, or the private scratch
  volume when no cache is admitted) and is never backed by a StateRoot mount or a
  host parent grant. Capture its candidate only after positive payload-descendant
  termination, but retain no publication authority in the worker: the controller
  streams bounded candidate data to Store only after native seed/unit cleanup is
  positively confirmed. Input overflow rejects before GO; candidate overflow
  rejects publication after command execution and preserves the last committed
  cache. On macOS, read/capture the mounted cache or scratch volume while its checked
  handle is live, then positively empty the coalition and detach storage before
  Store publication. An explicitly selected read-only host-cache subtree takes
  precedence: exclude that exact relative root before private archive traversal,
  never import host bytes, and preserve earlier private archive entries at that
  root when other cache paths are published. Uncertain command, storage, seed
  or unit cleanup never commits cache bytes.
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
- Linux toolchains mount at their exact host paths; host-cache grants mount
  under `/cache`, a separate quota-backed volume sized by the build-cache limit,
  when a cache view is admitted, and under private `/scratch` otherwise. Linux
  uses `sandbox_cache_root`; macOS links the same cache grants from its cache
  storage volume (`prepare_cache_links`), or from `scratch` when no cache limit
  is configured. Both use the same base for the payload's `CARGO_HOME`,
  `npm_config_cache` and `PIP_CACHE_DIR`. A toolchain target that equals, lies under or contains a path the
  profile mounts or creates is rejected before seed creation. Keep Linux
  `/lib` targeting `/usr/lib` and `/lib64` targeting `/usr/lib64`: mapping both to
  `/usr/lib` hides the x86-64 ELF loader even when the ARM64 native gates pass.
  Native fixtures must preserve the same loader layout as the production mount
  profile.
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
  admitted Run. Across Runs, a process may reuse a determined in-memory
  signature verdict only for the same path and an identical fresh metadata
  stamp; any drift reinspects the code. An undetermined Security result excludes
  the program for that admission only and is never reused. Never persist verdicts. Keep the catalogue inside the existing Task byte/entry bounds;
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

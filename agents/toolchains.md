# Runtime profiles

Load when changing `src/tools/toolchains.rs` or the folder-trust selection of
toolchains and host caches. The [Tool spec](../docs/specs/tools.md) owns
observable behavior; [Tools](./tools.md) and [Guard](./guard.md) own dispatch
and enforcement.

- Discover only the known per-manager directories with bounded enumeration
  and file metadata. Never execute a candidate or read package-manager
  configuration, project instructions or credential files during discovery.
  Run discovery only for initial consent or when `/permissions` opens the
  selector. Remembered trusted startup validates saved grant pins without
  enumerating optional home-managed directories, so an unrelated candidate
  overflow cannot block chat; the explicit selector reports its own bounded
  discovery failure.
- Saved grants stay pinned even when they no longer verify. Never filter them
  by host state at admission: Guard rechecks them before a command or MCP
  dispatch and rejects with `ChangedInput`, so a stale `cargo` cannot resolve to
  another toolchain. File and Skill Tools and chat keep working. Saving a
  confirmed selection verifies only fresh candidates and rejects any grant that
  overlaps the workspace, the account root or a runtime root.
- Tokens hash the exact path, device and inode for every kind, so a recreated
  cache or toolchain root is a new candidate and the saved pin shows as no longer
  detected.
- Admit only numeric `major.minor.patch` directory labels. Bind the exact
  manager label, absolute selected path and directory device/inode. The label
  identifies the selected directory; it does not prove the binary's version.
  Revalidate identity without following links; unsupported or changed roots
  fail closed.
- Python discovery accepts a version-specific regular executable or an
  in-root `python`/`python3` symlink whose single relative target is a
  version-specific regular executable in the same `bin` directory. Never
  follow an arbitrary alias or symlink chain.
- Host caches require a distinct explicit choice for each known cache root.
  Select only the exact cache subtree, read-only; never traverse to a parent
  cache/configuration directory. Separate configuration and credential files
  are excluded, but selected cache data may contain private package names,
  source, registry-response metadata or embedded credential URLs. Consent warns
  that a command or local MCP Tool can emit selected data through bounded
  output; that output is journaled and may enter a later Provider request.
  Contents are not loaded automatically and supply no authority.
- Keep candidate discovery metadata-only and bounded. Keep platform mounts,
  Seatbelt rules, process authority and native attestation in Guard; both
  Linux and macOS must reject stale identities before dispatch. Linux mounts
  each toolchain at its exact host path, so absolute shebangs resolve; that
  requires a private-propagation bootstrap and a bounded mount-table attestation
  before Ready and GO. See [Guard rules](./guard.md).

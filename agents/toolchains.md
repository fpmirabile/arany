# Runtime profiles

Load when changing `src/tools/toolchains.rs` or the folder-trust selection of
toolchains and host caches. The [Tool spec](../docs/specs/tools.md) owns
observable behavior; [Tools](./tools.md) and [Guard](./guard.md) own dispatch
and enforcement.

- Discover only the known per-manager directories with bounded enumeration
  and file metadata. Never execute a candidate or read package-manager
  configuration, project instructions or credential files during discovery.
- Admit only numeric `major.minor.patch` directory labels. Bind the exact
  manager label, absolute selected path and directory device/inode. The label
  identifies the selected directory; it does not prove the binary's version.
  Revalidate identity without following links; unsupported or changed roots
  fail closed.
- Host caches require a distinct explicit choice for each known cache root.
  Select only the exact cache subtree, read-only. Never traverse to a parent
  cache/configuration directory or load credentials. Cache data may reveal
  private package names, source or registry-response metadata, and never enters
  Provider context or supplies authority.
- Keep candidate discovery metadata-only and bounded. Keep platform mounts,
  Seatbelt rules, process authority and native attestation in Guard; both
  Linux and macOS must reject stale identities before dispatch.

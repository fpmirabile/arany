# Shared native Guard admission

Load for `src/tools/guard/admission.rs` and its Linux/macOS callers. The
[Tool contract](../docs/specs/tools.md#shared-native-capacity-and-settings) owns
settings, bounds, waiting and overflow behavior.

- One fixed user-account-root `guard` directory owns coordination across Session
  roots and processes. Admit private no-follow directories/files, owner, ACL,
  mode, regular type, single link and bounded content. Workspace input cannot
  choose this directory or its settings. Never create a database here.
- The checked zero-byte namespace lock serializes ticket registration, counting
  and reservation publication. Keep manager calls and other awaits outside it.
  Lock contention and polling retain original intent expiry and cancellation.
  Open existing files first, create missing locks exclusively, and reopen on
  `AlreadyExists`, preserving no-follow checks. Concurrent macOS no-follow
  `create(true)` returned `NotFound` in the gated first-creation corpus.
- Publish complete, locked lease records by atomic rename before exposure.
  Unpublished `.new` records cannot have launched effects and may be removed only
  under the namespace lock after validating the file and acquiring its dead lease.
  Counter publication precedes ticket publication; gaps are harmless, reuse is not.
- FIFO uses monotonic registration sequence, not UUID/date order. No new request
  bypasses registered waiters. Death releases the lease; expiry/cancellation
  removes a position without replay or renewed authority. Overflow has no second
  queue. Treat normal concurrent removal as absence and parse opened handles.
- Count live, recovering and retained reservations. macOS also counts all owned
  control roots, deduplicated by reservation UUID. An unlocked reservation whose
  root remains must not free image capacity. Linux releases only after positive
  native termination; uncertain startup remains reserved. Previous-boot records
  cannot refer to a live Linux unit from that boot.
  A nil boot identity is malformed, not evidence that a previous boot ended.
- Shared settings are user authority, independent of Session/workspace config.
  Validate finite ranges on every admission. Lowering a limit preserves existing
  owners; it restricts new admission. A settings error never widens capacity.
- Progress is a one-slot, lossy, intent-ID hint. It neither creates canonical
  Events nor enters Provider context, and clears on dispatch, error or cancellation.
  Terminal checks it against the current committed pending Tool.
- Cross-process capacity/FIFO evidence belongs to the admission corpus; actual
  resource cleanup belongs to each native unit owner. macOS evidence cannot
  establish Linux support. Review the shared algorithm on both targets.

# Shared native Guard admission

Load when changing shared admission, settings or reservation ownership in
`src/tools/guard/admission.rs` or its Linux/macOS callers. The
[Tool contract](../docs/specs/tools.md#shared-native-capacity-and-settings) owns
settings, bounds, waiting and overflow behavior.

- Use the fixed account root and checked [private filesystem handles](./private-fs.md).
  Files require no-follow, owner/mode/ACL, regular type, single link and bounded
  content. Coordination creates no database or history records.
- The checked zero-byte namespace lock serializes ticket registration, counting
  and reservation publication. Keep manager calls and other awaits outside it.
  Open existing files first, create missing locks exclusively, and reopen on
  `AlreadyExists`, preserving checks. Concurrent macOS no-follow `create(true)`
  can report `NotFound`. Parse opened handles; concurrent removal means absence.
- Publish complete, locked lease records by atomic rename before exposure.
  Unpublished `.new` records cannot have launched effects and may be removed only
  under the namespace lock after validating the file and acquiring its dead lease.
  Counter publication precedes ticket publication; gaps are harmless, reuse is not.
- FIFO uses the persisted monotonic registration sequence, not UUID/date order.
- Count macOS control roots independently of reservations, deduplicating UUIDs;
  a dead reservation whose root remains must not free image capacity. Linux
  reclamation requires checked seed deletion and, after launch was requested,
  positive native termination or validated previous-boot evidence. A boot change
  does not remove storage. Nil boot identities and uncertain startup cannot
  release capacity.
- Linux's version-2 reservation is a stable leased inode; atomically replace
  only its separate bounded seed metadata. Publish the planned location before
  exclusive mkdir and its checked identity before copying bytes. Preparing
  without a committed identity authorizes only empty-directory removal. Commit
  launch-requested before spawning and termination before storage cleanup.
  Create every snapshot directory with explicit private permissions, independent
  of the controller's umask, so ordinary copies remain admissible for cleanup.
- A reaper revalidates the named lease's inode/link count under the namespace
  lock after acquiring flock. Keep native awaits and recursive deletion outside
  that lock. Retire seed metadata and then the lease only after synchronized
  storage absence. Recovery Drop must not bypass its attempt/deadline budget.
  Reject unsupported records intact; historical cleanup is explicit maintenance.
- Route temporary wait hints through the existing callback. Engine and terminal
  own progress projection; admission never creates canonical Events.

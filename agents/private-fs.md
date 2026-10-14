# Private filesystem admission

Load when changing `src/private_fs.rs` or its native ACL adapter. Store and
Guard share this internal filesystem boundary; it has no history or Tool policy.

- Admit absolute normalized paths component by component through no-follow
  directory handles. Preserve checked ancestor identities for Workspace exclusion.
  New components use mode `0700`; existing leaves require the current owner and
  private permissions. Root-owned sticky temporary ancestors remain admissible.
  Symlinked ancestors reject too, including macOS `/tmp` and `/var` aliases.
  Concurrent creation may return `AlreadyExists`; still reopen through the same
  no-follow handle and apply every ordinary directory/owner/ACL check.
  Select physical paths such as `/private/tmp`; never canonicalize an untrusted
  supplied path to bypass this admission rule. Native fixtures use a physical
  temporary root, and user-selected State/account paths follow the same rule.
- Ordinary Workspace access uses a separate no-follow descriptor opener and
  keeps the existing Workspace policy: absolute component traversal without
  owner, mode, or ACL requirements on Workspace directories. Do not route an
  ordinary Workspace through private State admission; callers that need an
  exclusion check compare the opened Workspace identity against the admitted
  private root's checked ancestors.
- Opening a private directory creates no records, database or locks. Store owns
  those artifacts; Guard retains the admitted handle for its control-root lifetime.
- On macOS, check ACLs through opened descriptors. Private objects reject every
  allow entry; ancestors reject mutation grants. Deny-only ACLs remain admissible.
  Unknown entries, query errors and more than 128 entries fail closed. Linux
  retains its existing owner/mode checks; unsupported platforms reject admission.
- The native ACL adapter is an isolated unsafe exception. Borrowed descriptors
  stay live, entries borrow one validated allocation, and its native allocator
  frees it exactly once. Handle absent-ACL `ENOENT` and end-of-entry `EINVAL`
  only at their owning calls. Review ABI/lifetimes and run the native ACL corpus
  when changing it. Admission does not exclude same-UID or privileged attackers.
- Verify through the existing State admission corpus, native ACL corpus and
  activated Guard unit owner; preserve filesystem errors at each caller's boundary.

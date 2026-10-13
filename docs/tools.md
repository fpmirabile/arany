# Local coding tools

Attached startup asks for folder trust before account selection or setup unless that exact folder is already trusted. `/permissions` revisits that choice. A reviewed private grant can instead be selected with `--tools`:

```sh
target/debug/arany --tools
target/debug/arany exec --tools --provider openai --model gpt-5.4 "Inspect and fix the selected code"
```

Submitting work can consume Provider usage. The [Tool contract](./specs/tools.md) defines supported operations, enforcement and limits. The primary alone performs effects; commands/MCP use offline disposable project copies, while typed Edit/Write integrates approved changes.

## Folder trust and approvals

See the [trust and approval contract](./specs/tools.md#folder-trust-and-approvals) for remembered folder identity, Shift+Tab modes, one-use human approvals and AI-review fallback. Untrusted folders remain read-only.

## Private grants

Use the [minimal file-only configuration](./specs/tools.md#private-grants) to create owner-only `tools.json` outside the Workspace. `--state-dir` selects this configuration root without moving saved accounts. Review selected paths and resources before granting access; Tool observations may reach the selected Provider and private journal.

## Commands

Follow the [executable and input pin format](./specs/tools.md#commands). Pin resolved executable paths and current SHA-256 digests. Disposable command writes are discarded; use typed file operations to integrate project changes.

## Portable Skills

Install project Skills under `.agents/skills`. If a project lock exists without that directory, Arany advises `npx --yes skills@1.7.0 experimental_install`; it does not run the installer. See [Skill admission and private pins](./specs/tools.md#portable-skills) for supported metadata/resources and bounds.

Type `/` and use Left/Right to select Skills. Tab or the first Enter stages `$NAME `; a later Enter submits. `/settings` chooses tabs or a combined list. Browsing does not load Skill guidance or run effects; the model requests admitted guidance through the Skill Tool.

## Local MCP

Follow the [local MCP configuration](./specs/tools.md#local-mcp) for pinned server inputs, fixed arguments and allowed tool names. This offline stdio profile supports local data/computation rather than external APIs or credentialed services.

## Limits and failure behavior

On Linux and macOS, edit the private user-level `guard/limits.json` to change
simultaneous Guard units and waiting Tools.
The [shared capacity/settings contract](./specs/tools.md#shared-native-capacity-and-settings)
gives exact paths, JSON, defaults, ranges and waiting behavior.

The [limits and recovery contract](./specs/tools.md#limits-and-failure-behavior) describes quotas, exact file preconditions and uncertain effects. Inspect `arany show --output text SESSION_ID` and actual files before an intentional retry. Historical intents never execute on replay or resume.

## Native enforcement and verification

Check the [Linux prerequisites](./specs/tools.md#linux-enforcement-and-compatibility) or [macOS prerequisites and compatibility](./specs/tools.md#macos-enforcement-and-compatibility). Both native profiles enforce offline isolation and owned cleanup, with the resource guarantees specified in their contracts. Missing enforcement rejects without an unsandboxed fallback. [Next steps](../NEXT_STEPS.md) distinguishes demonstrated checks from remaining user-owned verification.

## Historical Linux seed maintenance

Current seeds are recovered on subsequent Tool admission under the
[ownership contract](./specs/tools.md#linux-host-seed-ownership). Older unrecorded
`/tmp/arany-tools-*` directories and version-1 Linux reservations need one explicit
maintenance interval. There is no automatic migration or prefix-based cleaner.
The old seed UUID and unit UUID were independent, so names, modes and timestamps
cannot establish their association or authorize deletion.

1. Stop Arany for the selected Linux account and prevent new launches. Establish
   termination of its owned native units, including descendants. An absent unit
   alone cannot prove an interrupted launch ended. If inactivity remains uncertain,
   reboot the maintenance host and clean before restarting Arany. For a disposable
   Docker fixture, stopping its container establishes inactivity of its processes;
   it does not remove files in its writable layer or volumes.
2. Inventory metadata only for explicitly selected candidate roots and obsolete
   reservation/publication files. Record each absolute path, checked parent and
   object device/inode, UID, mode and link count. Bound the inventory to 64 roots
   per batch and the existing seed traversal limit. Do not read copied payloads.
   Obtain authorization for these exact current objects; a prefix is only a
   discovery hint. Record settings and unrelated canaries to preserve.
3. Reopen selected parents component by component without following links.
   Revalidate every selected identity immediately before deletion. Acquire the
   checked dead lease for each selected reservation; a live flock refuses
   maintenance. Require private account/root permissions and current ownership.
   Refuse replacements, symlinks, hard-linked files, special objects and mounts.
4. Delete the approved tree through pinned directory handles, with no-follow and
   no-cross-mount opens, bounded traversal and identity revalidation before each
   unlink/rmdir. Synchronize the parent after verified root absence. The existing
   Guard deletion primitive owns this behavior; do not replace it with a wildcard
   `rm -rf`. Retire only explicitly selected obsolete records under the account's
   namespace lock, after storage absence and native inactivity. Preserve
   `limits.json`, counters, other reservations and unrelated account data.
5. Verify each selected root/record absent and preserved metadata/canaries intact.
   Report any refused or partially removed object as unresolved. Resume Arany only
   after that final inventory; completion applies to this host/account, not other
   installations or secure media erasure.

The disposable maintenance scenario uses the same checked deletion primitive,
independent historical UUIDs, an explicit selected identity, a live-lease refusal,
a replacement and unrelated canaries. Run it on Linux with:

```sh
cargo test --lib tools::guard::admission::tests::historical_seed_maintenance_removes_only_selected_inactive_objects -- --exact
```

That command deletes only test-owned synthetic fixtures. It does not inventory or
remove an existing user's installation.

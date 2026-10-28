# Local coding tools

Attached startup asks for folder trust before account selection or setup unless that exact folder is already trusted. `/permissions` revisits that choice. A reviewed private grant can instead be selected with `--tools`:

```sh
target/debug/arany --tools
target/debug/arany exec --tools --provider openai --model gpt-5.4 "Inspect and fix the selected code"
```

Submitting work can consume Provider usage. The [Tool contract](./specs/tools.md) defines supported operations, enforcement and limits. The primary alone performs effects; commands/MCP use offline project copies. Command changesets and typed Edit/Write publish only through approved, validated mutations; MCP writes remain disposable.

## Folder trust and approvals

See the [trust and approval contract](./specs/tools.md#folder-trust-and-approvals) for remembered folder identity, Shift+Tab modes, one-use human approvals and AI-review fallback. Untrusted folders remain read-only.

The primary may request a host command at any time. Review the exact command before approving it once or, when offered, for this live Session. Privilege-escalation commands are handed to you to run by hand. The [primary host-command contract](./specs/tools.md#primary-host-commands) defines approval scope, revocation and the inactive legacy rules file. To forbid host commands, add `"deny": ["command_unguarded"]` to `permissions.json`.

## Private grants

Use the [minimal file-only configuration](./specs/tools.md#private-grants) to create owner-only `tools.json` outside the Workspace. `--state-dir` selects this configuration root without moving saved accounts. Review selected paths and resources before granting access; Tool observations may reach the selected Provider and private journal.

## Commands

Follow the [executable and input pin format](./specs/tools.md#commands). Pin resolved executable paths and current SHA-256 digests. Command file changes return as bounded changesets and require a separate validated integration receipt; see the command changeset contract for limits and approval.

## Network access

Commands are offline unless `tools.json` has a [`network` grant](./specs/tools.md#network-grants-and-egress-proxy). Commands then reach the listed HTTPS domains through a Guard proxy; `cargo` and `npm` honor the preset proxy variables, and a tool that ignores them cannot connect. Refused connections appear in the command result. Add a refused domain in `/permissions` > Network domains; it applies from the next Run. Allowed hosts can still receive any data a command sends them.

## Portable Skills

Install project Skills under `.agents/skills`. If a project lock exists without that directory, Arany advises `npx --yes skills@1.7.0 experimental_install`; it does not run the installer. See [Skill admission and private pins](./specs/tools.md#portable-skills) for supported metadata/resources and bounds.

Use [slash completion](./specs/terminal.md#interactive-commands-and-completion) to invoke an admitted Skill. The [Portable Skills contract](./specs/tools.md#portable-skills) owns loading and attachment.

## Local MCP

Follow the [local MCP configuration](./specs/tools.md#local-mcp) for pinned server inputs, fixed arguments and allowed tool names. The stdio profile is offline and supports local data/computation rather than external APIs or credentialed services.

On Linux an `mcp` entry with `"url": "https://…"` and `"auth": "oauth"` is a [remote server](./specs/tools.md#remote-mcp-and-oauth) reached through the egress proxy. Run `arany mcp login <server>`, open the printed URL and approve; `arany mcp logout <server>` revokes and removes the saved token. A call without a valid token fails with an `unauthorized` result naming that command.

## Limits and failure behavior

On Linux and macOS, edit the private user-level `guard/limits.json` to change
simultaneous Guard units and waiting Tools.
The [shared capacity/settings contract](./specs/tools.md#shared-native-capacity-and-settings)
gives exact paths, JSON, defaults, ranges and waiting behavior.

The [limits and recovery contract](./specs/tools.md#limits-and-failure-behavior) describes quotas, exact file preconditions and uncertain effects. Inspect `arany show --output text SESSION_ID` and actual files before an intentional retry. Historical intents never execute on replay or resume.

## Native enforcement and verification

Check the [Linux prerequisites](./specs/tools.md#linux-enforcement-and-compatibility) or [macOS prerequisites and compatibility](./specs/tools.md#macos-enforcement-and-compatibility). Both native profiles enforce offline isolation and owned cleanup, with the resource guarantees specified in their contracts. Missing enforcement rejects without an unsandboxed fallback. [Next steps](../NEXT_STEPS.md) distinguishes demonstrated checks from remaining user-owned verification.

## Historical Linux seed maintenance

Current seeds recover on subsequent Tool admission under the
[ownership contract](./specs/tools.md#linux-host-seed-ownership). Unrecorded
`/tmp/arany-tools-*` roots and version-1 Linux reservations require explicit
maintenance; no automatic migration or prefix cleaner exists. Historical seed
and unit UUIDs were independent: names, modes and timestamps cannot establish
association or authorize deletion.

1. Stop Arany and prevent launches for the selected Linux account. Prove owned
   units and descendants terminated; an absent unit cannot settle an interrupted
   launch. If uncertain, reboot and clean before restarting. Stopping a disposable
   Docker container proves process inactivity but preserves layer/volume files.
2. Inventory only selected root/reservation/publication metadata: absolute path,
   checked parent, object device/inode, UID, mode and link count. Limit batches to
   64 roots and the existing seed traversal bound; read no copied payloads.
   Authorize these exact current objects; prefixes only aid discovery. Record
   settings and unrelated canaries to preserve.
3. Reopen parents component by component without following links; revalidate
   each identity immediately before deletion. Acquire each reservation's checked
   dead lease; live flocks refuse maintenance. Require private account/root
   permissions and current ownership. Refuse replacements, symlinks, hard-linked
   files, special objects and mounts.
4. Use the Guard's checked deletion primitive through pinned directory handles:
   no-follow/no-cross-mount opens, bounded traversal and identity checks before
   every unlink/rmdir. Never substitute wildcard `rm -rf`. Synchronize the parent
   after verified root absence. Retire only selected obsolete records under the
   account namespace lock after storage absence and native inactivity. Preserve
   `limits.json`, counters, other reservations and unrelated data.
5. Verify selected roots/records absent and preserved metadata/canaries intact.
   Report refused or partial removal as unresolved; resume only after this final
   inventory. Completion covers this host/account, not other installations or
   secure media erasure.

Remaining host cleanup is tracked in [next steps](../NEXT_STEPS.md#account-recovery-and-native-guard-follow-ups).

## Web fetch and search

The model may call `web_fetch` for one URL: a host on the [`network` grant](./specs/tools.md#network-grants-and-egress-proxy) runs directly and any other host asks you to approve it for that call. It returns text (HTML is reduced to text, never executed), cut at [`WEB_FETCH_CHARS`](./specs/limits.md#web_fetch_chars) characters and marked when cut. Redirects are refused. To add `web_search`, put `"web_search": {"endpoint": "https://api.search.brave.com/res/v1/web/search", "key": "ARANY_WEB_SEARCH_KEY"}` in `tools.json` and export that variable; it is read only when a search starts and is never stored. Both are Linux only and their results are untrusted data. To offer them to read-only researchers, set `"child_web": true`; researcher fetches remain inside the parent's listed domains and cannot request additional hosts. Details are in the [Tools contract](./specs/tools.md#web-fetch-and-web-search).

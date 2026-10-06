# Local coding tools

In attached chat, select an account/model, then explicitly trust the folder when prompted. `/permissions` revisits that choice. A reviewed private grant can instead be selected with `--tools`:

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

Install project Skills under `.agents/skills`. If a project lock exists without that directory, Arany advises `npx skills install`; it does not run the installer. See [Skill admission and private pins](./specs/tools.md#portable-skills) for supported metadata/resources and bounds.

Type `/` and use Left/Right to select Skills. Tab or the first Enter stages `$NAME `; a later Enter submits. `/settings` chooses tabs or a combined list. Browsing does not load Skill guidance or run effects; the model requests admitted guidance through the Skill Tool.

## Local MCP

Follow the [local MCP configuration](./specs/tools.md#local-mcp) for pinned server inputs, fixed arguments and allowed tool names. This offline stdio profile supports local data/computation rather than external APIs or credentialed services.

## Limits and failure behavior

The [limits and recovery contract](./specs/tools.md#limits-and-failure-behavior) describes quotas, exact file preconditions and uncertain effects. Inspect `arany show --output text SESSION_ID` and actual files before an intentional retry. Historical intents never execute on replay or resume.

## Linux enforcement and verification

Check the [native prerequisites and compatibility boundary](./specs/tools.md#linux-enforcement-and-compatibility). Missing enforcement rejects without an unsandboxed fallback. Native macOS containment is not implemented. [Next steps](../NEXT_STEPS.md) distinguishes demonstrated checks from remaining user-owned verification.

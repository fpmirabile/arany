# Local coding tools

This contract owns Tool grants, guarded effects, runtime Skills and local MCP. [Terminal](../terminal/README.md) owns completion, settings and consent presentation; [execution](../execution/README.md) owns Run coordination, capacity admission and [effect outcomes](../execution/events-and-outcomes.md#effect-outcomes). Editing constraints: [security rules](../../../agents/security.md).

A primary AgentRun can use file operations, commands, portable Skills and local stdio MCP tools. This is an explicit, offline local foundation, not unrestricted host shell access. [Folder trust](./trust-and-approvals.md#folder-trust-and-approvals) gates Tools on attached startup; without explicit private Tool configuration, untrusted folders stay read-only. Explicit process selections and headless Runs opt into reviewed configuration with `--tools`, which adds no paid compatibility check.

Folder consent and native Tool capability are independent. Without an implemented enforcer, attached chat runs without Tools even after trust is remembered; `/permissions` offers the same consent choices, Shift+Tab reports the native restriction without enabling authority, and explicit `--tools` rejects. Trust never supplies missing OS protection: only approved [primary host commands](./host-commands.md#primary-host-commands) and [typed host](./git.md#human-approved-host-git) operations run outside the Guard, and [in-process file operations](./file-operations.md#in-process-file-operations) run only once native protection admits Tools.

Supported operating systems provide equivalent file operations, commands, Skills and local MCP with the same permission, isolation, cancellation and durable-result contract. Typed Git needs a Git on the user's `PATH` ([availability](./git.md#typed-git-reads)); resource guarantees differ by native profile as defined below. [Next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release) records the native, platform, live and human checks still required before claiming support.

Native adapters use [automatic Tool selection and plain text answers](../provider/encoding-and-streaming.md#structured-outcomes-and-tool-encoding), translating one semantic proposal at a time. Exact [custom profiles](../provider/routing-and-usage.md#custom-destinations-and-routing) receive the same Tools. Compatibility with a real selected model requires live evidence.

The generated function and outcome schemas offer file reads and search plus only catalog-enabled mutations, commands, Skills, MCP operations, typed Git reads and, with `write`, [typed Git writes](./git.md#typed-git-writes). Command, Skill and MCP-server identifiers are constrained to configured names. An unparsable or oversized catalog offers only read-only file operations and the primary's optional plan function. Catalog text guides generation but supplies no authority; the pinned router and Guard admit every proposal independently.

The optional `arany_plan` function records Engine-owned Goal state, separate from effect Tools; [Goals](../goals.md#initial-plan-contract-and-success-receipt) owns its admission, scope, success and replay.

Topic files:

- [Authority, trust and approvals](./trust-and-approvals.md): Authority and effect admission; Folder trust and approvals.
- [Permission modes and rules](./permissions.md): Permission modes, rules and hooks; Modes and approval decisions; Permission rules; Permission limits.
- [Hooks and supervisor adapter](./hooks.md): Hook execution; Supervisor adapter.
- [Private grants and host commands](./host-commands.md): Private grants; Enforcement failure causes; Primary host commands.
- [Commands, build cache and network](./commands-and-cache.md): Commands; Persistent Workspace build cache; Network grants and egress proxy; Local storage inventory.
- [Command inputs, changesets and checkpoints](./changesets-and-checkpoints.md): Command input selection; Command changesets; Checkpoints and rewind.
- [Skills and MCP](./skills-and-mcp.md): Portable Skills; Local MCP; Extension and transport admission.
- [Agent definitions and editors](./editors.md): Agent definition parser admission; Editor Tool steps.
- [Git](./git.md): Typed Git reads; Human-approved host Git; Typed Git writes.
- [File operations](./file-operations.md): In-process file operations; Typed delete and move; Atomic single-file MultiEdit.
- [Limits and failure behavior](./limits-and-failures.md): Limits and failure behavior.
- [Native Guard enforcement](./native-guard.md): macOS enforcement and compatibility; Shared native capacity and settings; Linux enforcement and compatibility.

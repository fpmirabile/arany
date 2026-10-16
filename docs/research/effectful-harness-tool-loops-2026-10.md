# Effectful harness tool loops

**Reviewed:** 2026-10-04
**Status:** retained primary-source comparison, not implementation or containment proof
**Scope:** Codex, official Claude Code documentation, and current upstream Pi; functional beta 1 tools and commands, without a new UX requirement

The [Tool spec](../specs/tools.md), [Execution spec](../specs/execution.md) and [security gate](../../agents/security.md#effectful-execution-gate) own the accepted lifecycle. This note retains source comparisons behind the adopted loop; implementation steps and acceptance requirements live in those owners. No provider call, account operation, installer, foreign project code or tool execution was performed for this research.

## What the primary sources establish

### Codex

Codex exposes a Thread/Turn/Item lifecycle: resumable stored threads, separate command execution, file changes, MCP calls and collaboration calls, with started/completed Items. Completed Items are authoritative over streaming deltas. Command/file approvals are correlated with thread, turn and item identities, and can decline or cancel work. Turn interruption produces an interrupted terminal state. This is evidence for a coherent effect lifecycle and persisted inspection, not proof of exactly-once external effects or arbitrary descendant cleanup. [Official app-server contract](https://learn.chatgpt.com/docs/app-server#items), [approvals](https://learn.chatgpt.com/docs/app-server#approvals), [thread and turn lifecycle](https://learn.chatgpt.com/docs/app-server#api-overview).

Codex explicitly separates approval policy from sandbox mode: approval decides when an action needs a decision, while OS enforcement constrains what commands can technically do. Its current Linux implementation uses Bubblewrap or a bundled helper requiring user namespaces; macOS uses Seatbelt. These are platform adapters, not a universally identical implementation. The documentation does not establish an Arany-ready attestation protocol. [Approval/security contract](https://learn.chatgpt.com/docs/agent-approvals-security#sandbox-and-approvals), [platform sandbox](https://learn.chatgpt.com/docs/sandboxing#what-the-sandbox-does).

Codex subagents inherit the selected sandbox/permission settings; unavailable fresh approval in noninteractive work fails back to the parent. The docs recommend read-heavy parallel delegation and warn about concurrent writers. This supports isolated child context and inherited boundaries, not copying independently configurable child authority into Arany. [Official subagent controls](https://learn.chatgpt.com/docs/agent-configuration/subagents#approvals-and-sandbox-controls), [parallel work guidance](https://learn.chatgpt.com/docs/agent-configuration/subagents#why-subagent-workflows-help).

OpenAI's API contract separately defines function calls, `call_id`-matched outputs, explicit strict JSON Schemas and optional single-call selection. Native reasoning/tool continuations may require returned reasoning items as well as outputs. That protocol cannot be approximated by dropping all prior assistant items and sending a plain result string. The API documentation alone does not prove Codex's internal execution implementation. [Function calling](https://developers.openai.com/api/docs/guides/function-calling#handling-function-calls), [strict mode](https://developers.openai.com/api/docs/guides/function-calling#strict-mode).

### Claude Code: documented behavior only

The official architectural guide describes repeated context gathering, action and verification, with results feeding the next model decision. It documents file/search/execution tools, local plaintext JSONL storage of messages/tool uses/results, separate child contexts, and interruption of a running tool. These are public product behaviors, not inspected closed-source implementation or a crash-durability guarantee. [How Claude Code works](https://code.claude.com/docs/en/how-claude-code-works#the-agentic-loop), [session persistence](https://code.claude.com/docs/en/how-claude-code-works#work-with-sessions).

Permissions are enforced by Claude Code rather than prompt text. Deny rules precede ask and allow rules. Manual mode distinguishes ordinary reads from edits and commands; accepted file changes and remembered command permissions have different scopes. Arany should adopt explicit policy evaluation and denial precedence, not command-pattern matching as its security boundary. [Official permissions](https://code.claude.com/docs/en/permissions#manage-permissions).

The documented Bash sandbox applies to commands and descendants, but **not** file tools, MCP servers or hooks. It can permit excluded commands and unsandboxed retries. Its network allowlist is not payload inspection and does not establish a data-disclosure guarantee. Consequently, a Claude-like Bash sandbox is not evidence that every Arany effect is mediated; Arany's router/Guard must cover every Tool path and reject unsupported enforcement rather than silently retry outside it. [Sandbox scope](https://code.claude.com/docs/en/sandboxing#what-the-sandbox-restricts), [documented limitations](https://code.claude.com/docs/en/sandboxing#limitations).

Claude's documented child definitions can restrict tools, permissions, MCP and turn counts. The general-purpose and read-only Explore/Plan agents illustrate using one loop with differing grants instead of different engines. This does not authorize project Markdown to grant Arany permissions. [Official subagent configuration](https://code.claude.com/docs/en/sub-agents#control-subagent-capabilities).

The Messages API returns `tool_use` blocks and expects `tool_use_id`-matched `tool_result` blocks in subsequent input. Client tools execute in the caller's application even when Anthropic publishes their schema. This is a wire protocol, not the full Claude Code harness. [Official tool-use protocol](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview#client-tools).

### Pi: source-level evidence

The former `badlogic/pi-mono` repository redirects to `earendil-works/pi`. Current upstream still describes a minimal extensible harness without built-in subagents/plan mode. The following `main` links were fetched on the review date; they are mutable research references, **not** pinned runtime dependencies. Old fork READMEs do not establish current upstream behavior. [Current upstream](https://github.com/earendil-works/pi), [coding-agent README](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md).

`agent-loop.ts` appends assistant calls and correlated `toolResult` messages, then infers again. It validates prepared arguments, supports before/after-call hooks and sequential/parallel batches, retains result order for parallel work, returns unknown/invalid/blocked/throwing tools as errors, and passes abort signals. A truncated model response fails its calls instead of executing possibly incomplete arguments. Its nested `runToolCall` uses the same preparation/hooks. Hooks are useful routing structure, not an OS enforcement boundary. [Current loop source](https://github.com/earendil-works/pi/blob/main/packages/agent/src/agent-loop.ts).

The built-in `read` supports offset/limit and explicit truncation notices, but reads the complete local file before selecting text. `edit` applies unique, disjoint replacements against the original file, preserves line endings and serializes mutation by pathname. `write` creates/overwrites and creates parents. These are useful model-facing semantics; their ordinary pathname access does not satisfy Arany's pinned-handle, no-follow or aggregate read bounds. [Read source](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/read.ts), [edit source](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/edit.ts), [write source](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/write.ts).

Pi's shell operations stream both channels, use detached process ownership on Unix and call tree termination on abort/timeout. The schema permits an omitted timeout with no default; the output accumulator retains a bounded display tail while spooling additional bytes. Thus display truncation alone proves neither finite runtime nor a disk quota. Arany needs independent execution, capture, artifact and aggregate limits. [Shell source](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/bash.ts), [output accumulation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/output-accumulator.ts).

Pi's session manager persists append-only JSONL entries with parent relationships and derives a compaction-aware active branch. Its parser explicitly tolerates malformed lines and its persistence code is not Arany's strict SQLite commit-before-feedback contract. Borrow the call/result context shape, not weaker recovery admission. [Session manager source](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/session-manager.ts), [sessions/context](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/sessions.md).

Pi's current security guide states that normal tool execution uses the caller's OS permissions without approval for every call, project trust controls startup resource loading rather than runtime capabilities, and extension code executes in the Pi process. It separately documents partial startup exceptions. Arany should not import a remembered path-trust decision as a root/executable/network grant. [Current security contract](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/security.md).

Pi Skills advertise names/descriptions and load full instructions when needed; scripts/references/assets may accompany them. This is progressive disclosure of guidance, not an execution privilege. [Current Skills documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/skills.md).

## Retained design implications

The comparisons support one Engine-owned effect lifecycle, correlated call/results
and a separate enforcement boundary. Provider adapters translate complete native
continuations; they do not execute effects. Approval UX, process cancellation and
persisted history answer different questions and cannot stand in for containment
or exactly-once execution.

The original implementation sequence and regression checklist are now absorbed
by the [Tool spec](../specs/tools.md), [architecture](../architecture/system-overview.md)
and [testing rules](../../agents/testing.md). Current uncertainty and unperformed
native/live checks belong in [next steps](../../NEXT_STEPS.md). These comparisons
prove no Arany platform support, remote rollback, paid compatibility or package
provenance.

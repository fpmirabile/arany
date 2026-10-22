# macOS Guard crash recovery in other agent harnesses

Date: 2026-10-12. Status: research and proposed design assessment; not approval to change implementation or evidence of runtime conformance.

## Answer

The inspected harnesses do not establish an equivalent macOS recovery contract for Arany's combination of one-use launchd jobs, kernel resource coalitions and mounted fixed-capacity images. Their local cleanup mechanisms mostly execute inside the still-running owner. This makes them useful examples of ordinary lifecycle cleanup, but insufficient evidence that Arany should replace its persisted ownership and later reconciliation with `Drop`, signal handlers or process-group killing. See the source-specific comparison below.

**Assessment, not a demonstrated guarantee:** keeping bounded recovery inside the existing macOS Guard is reasonable while Arany retains disposable mounted storage and its descendant-containment contract. The important question is whether every interruption leaves enough validated ownership evidence for the next admission, without authorizing deletion or signaling from a filename, stale PID or stale disk number. The current [Tool contract](../specs/tools.md#macos-enforcement-and-compatibility), [unit recovery](../../crates/arany/src/tools/guard/macos/unit.rs) and [storage recovery](../../crates/arany/src/tools/guard/macos/storage.rs) explicitly address those concerns. Peer adoption alone cannot validate their race handling.

## Method and pinned sources

Public source was read as inert text; no peer harness was installed, cloned or executed. GitHub's commit API supplied these revisions on the research date. Claims below refer to those revisions rather than cached branch pages:

| Project | Revision inspected | Commit date (UTC) |
| --- | --- | --- |
| OpenAI Codex | [`6c372fbef45f730b6d8aa727b0ec8332365588db`](https://github.com/openai/codex/commit/6c372fbef45f730b6d8aa727b0ec8332365588db) | 2026-10-12 |
| Anthropic sandbox-runtime | [`5507fb0e639c00f845554336ece62e27af5b1bb7`](https://github.com/anthropics/sandbox-runtime/commit/5507fb0e639c00f845554336ece62e27af5b1bb7) | 2026-10-10 |
| OpenCode | [`bcf23b6775c79a429b1511b255d6d88ab0248d4a`](https://github.com/anomalyco/opencode/commit/bcf23b6775c79a429b1511b255d6d88ab0248d4a) | 2026-10-12 |
| Pi | [`11f99114506ad2cc759cb389663edaf7546cbe8c`](https://github.com/earendil-works/pi/commit/11f99114506ad2cc759cb389663edaf7546cbe8c) | 2026-10-12 |

Anthropic's official Claude Code documentation is a living page, retrieved on the research date. It identifies sandbox-runtime as the shell-sandbox implementation; it does not expose Claude Code's complete internal process/resource ownership implementation. This report therefore distinguishes the published product contract from the open library instead of attributing all library behavior to every Claude Code release. [Claude Code sandbox documentation](https://code.claude.com/docs/en/sandboxing).

## Comparison

“Not established” means the inspected lifecycle and cited contract do not prove the guarantee; it is not a claim that no other module, product or private implementation has it.

| Harness / inspected path | Ordinary cleanup demonstrated | macOS owner `SIGKILL` recovery established? | Persistent-resource comparison |
| --- | --- | --- | --- |
| Codex local stdio MCP | New process group; explicit termination and `Drop` termination. [Launcher](https://github.com/openai/codex/blob/6c372fbef45f730b6d8aa727b0ec8332365588db/codex-rs/rmcp-client/src/stdio_server_launcher.rs) | Not established by these owner-side paths; parent-death signaling helper is Linux-only. [Process groups](https://github.com/openai/codex/blob/6c372fbef45f730b6d8aa727b0ec8332365588db/codex-rs/utils/pty/src/process_group.rs) | Inspected macOS scratch handling grants access to host temporary paths; it does not own Arany-like mounted scratch images. [Scratch rules](https://github.com/openai/codex/blob/6c372fbef45f730b6d8aa727b0ec8332365588db/codex-rs/sandboxing/src/seatbelt_scratch.rs) |
| Claude Code / sandbox-runtime | Library reset on exit/SIGINT/SIGTERM; CLI forwards signals to its child. [Manager](https://github.com/anthropics/sandbox-runtime/blob/5507fb0e639c00f845554336ece62e27af5b1bb7/src/sandbox/sandbox-manager.ts), [CLI](https://github.com/anthropics/sandbox-runtime/blob/5507fb0e639c00f845554336ece62e27af5b1bb7/src/cli.ts) | Not established by these handlers; full product internals not audited | Seatbelt command wrapper plus host temp access/proxies, not a demonstrated equivalent of persistent launchd/image reconciliation. [macOS wrapper](https://github.com/anthropics/sandbox-runtime/blob/5507fb0e639c00f845554336ece62e27af5b1bb7/src/sandbox/macos-sandbox-utils.ts) |
| OpenCode shell service | Scoped process handle, timeout kill with escalation, recurring output-file retention. [Shell service](https://github.com/anomalyco/opencode/blob/bcf23b6775c79a429b1511b255d6d88ab0248d4a/packages/core/src/shell.ts) | Not established by scope finalization | Output-file garbage collection is comparable as housekeeping only; the project explicitly disclaims an agent sandbox. [Security contract](https://github.com/anomalyco/opencode/blob/bcf23b6775c79a429b1511b255d6d88ab0248d4a/SECURITY.md) |
| Pi built-in shell | Tracks detached child PIDs; timeout/abort/group kill; interactive shutdown handles SIGTERM/SIGHUP. [Shell helper](https://github.com/earendil-works/pi/blob/11f99114506ad2cc759cb389663edaf7546cbe8c/packages/coding-agent/src/utils/shell.ts), [interactive lifecycle](https://github.com/earendil-works/pi/blob/11f99114506ad2cc759cb389663edaf7546cbe8c/packages/coding-agent/src/modes/interactive/interactive-mode.ts) | Not established by the in-memory PID set and handlers | Inspected local shell launches in the host cwd with inherited/default shell environment. It does not establish a mounted-image ownership contract. [Built-in shell](https://github.com/earendil-works/pi/blob/11f99114506ad2cc759cb389663edaf7546cbe8c/packages/coding-agent/src/core/tools/bash.ts) |

## Implications for Arany

Apple specifies that SIGKILL cannot be caught or ignored. A Node exit/signal handler therefore cannot cover that failure; neither does Rust code that requires the killed process to execute. A later owner or a surviving independent owner must handle any remaining resources. [Apple signal semantics](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/sigaction.2.html), [Node signal events](https://nodejs.org/api/process.html#signal-events).

Apple also specifies that advisory locks coordinate cooperating processes, that duplicate/forked descriptors reference the same lock, and that the last close releases it. Process exit frees descriptors. This supports a liveness lock only when unintended processes cannot retain it; it does not authenticate persisted cleanup targets. [Apple flock semantics](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/flock.2.html), [Apple close semantics](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/close.2.html).

The [Tool contract](../specs/tools.md#macos-enforcement-and-compatibility) owns
recovery ordering and native acceptance scenarios. Source inspection does not
establish their execution or platform support.

## Alternatives and limits

These are proposed choices for design discussion, not approved implementation work:

- **Keep bounded admission recovery:** uses the existing Guard boundary and durable records. It permits retained resources until the next admission, and ambiguous/older roots can still require manual inspection. This is the present contract. [Tool contract](../specs/tools.md#macos-enforcement-and-compatibility).
- **Add an independent persistent reaper:** could shorten the interval before cleanup while Arany is stopped, but adds a service lifecycle, authority, installation and its own crash recovery. It still needs the same ownership and mount-validation evidence. This is an architectural inference; none of the compared macOS flows establishes a drop-in equivalent.
- **Use ordinary host temp directories and process groups:** resembles the inspected peers' simpler local execution paths, but does not supply Arany's fixed filesystem capacities or current descendant-containment contract. Adopting it would require an explicit product/security contract decision, rather than a cleanup refactor. [Arany guarantees](../specs/tools.md#macos-enforcement-and-compatibility), [Codex scratch policy](https://github.com/openai/codex/blob/6c372fbef45f730b6d8aa727b0ec8332365588db/codex-rs/sandboxing/src/seatbelt_scratch.rs), [Pi group cleanup](https://github.com/earendil-works/pi/blob/11f99114506ad2cc759cb389663edaf7546cbe8c/packages/coding-agent/src/utils/shell.ts).

## Evidence limits

Apple's [Disk Arbitration queue](https://github.com/apple-oss-distributions/DiskArbitration/blob/main/diskarbitrationd/DAQueue.c)
and [asynchronous requests](https://github.com/apple-oss-distributions/DiskArbitration/blob/main/diskarbitrationd/DARequest.c)
(reviewed 2026-10-12, mutable sources) do not establish DiskImages' pending-attach
barrier. Caller death alone is not cancellation proof. The [storage owner](../../crates/arany/src/tools/guard/macos/storage.rs)
and [native scenarios](../specs/tools.md#acceptance-scenarios-and-evidence-owners)
own reconciliation and verification. Cooperative async deadlines also do not
interrupt synchronous filesystem calls; no measured ordinary-host overrun was
established by the review.

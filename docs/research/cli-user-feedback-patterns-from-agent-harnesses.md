# CLI user-feedback patterns from agent harnesses

**Status:** primary-source comparison and beta recommendation  
**Retrieval date:** 2026-09-29  
**Scope:** OpenAI Codex CLI, Anthropic Claude Code, OpenCode, Google Gemini CLI, goose, and Aider.

## Question and boundary

What do established coding-agent harnesses do especially well in terminal feedback that Arany should reuse rather than reinvent?

The original comparison assumed append-only output with no TUI. The focused [beta terminal report](./beta-terminal-interface-and-multi-agent-feedback.md) supersedes that presentation constraint with a durable inline scrollback-first Session, accessible linear mode, and deterministic `exec`/`show`, while preserving the source comparison, channel separation, durability, child attribution, and recovery conclusions below.

Claims labeled **Fact** come from a linked first-party document or official source repository. **Inference** identifies a conclusion for Arany rather than a vendor guarantee. Absence from reviewed documentation is an evidence gap, not proof that a feature does not exist.

## Executive conclusion

Arany should reuse four mature contracts:

1. Reserve human-mode `stdout` for the root's final answer and `stderr` for append-only progress and diagnostics.
2. Make machine mode a typed JSONL event stream with stable identities, lifecycle states, usage, errors, and one terminal outcome.
3. Attribute every worker transition and attention request to a stable worker identity while keeping the root responsible for the final answer.
4. Persist before presenting: every factual progress line must be a rendering of a committed SQLite Event, and `show` must deterministically replay the same truth.

The products differ mainly in presentation. Codex has the cleanest default stdout/stderr contract. Claude has the strongest worker attribution, retry, cost, and stream correlation. OpenCode has useful child-session and event APIs but a topology Arany should not copy. Gemini has the clearest headless event/exit contract and compact task states. goose has unusually explicit inline subagent attribution and result summaries but unacceptable silent worker-loss behavior. Aider has humane interruption and recovery affordances but no reviewed structured event contract suitable as Arany's automation boundary.

## Product evidence

### OpenAI Codex CLI

**Fact.** `codex exec` streams human progress to `stderr` and writes only the final agent message to `stdout`. `--json` changes `stdout` into JSONL containing lifecycle and work-item events such as `thread.started`, `turn.started`, `turn.completed`, `turn.failed`, `item.*`, and `error`; the completion event includes token usage. `--output-last-message` separately captures the final response, and `--ephemeral` suppresses rollout persistence. [OpenAI: non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode)

**Fact.** Interactive Codex can resume or fork saved sessions. Noninteractive Codex can resume by session ID or `--last`. The CLI reference recommends pairing JSON progress with a separately captured final message in CI. [OpenAI: developer commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli)

**Fact.** `/agent` lets the user inspect an active subagent thread; the main thread gathers worker results into its final response. A user can ask the main agent to steer or stop a worker. Approval requests from inactive workers surface with the source-thread label. In noninteractive flows, work requiring a fresh approval fails and is reported back to the parent instead of waiting invisibly. [OpenAI: subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)

**Fact.** Codex separates the sandbox's effective technical boundary from the approval policy. Read-only, never-ask is a documented unattended combination, while bypassing both protections is explicitly dangerous. Optional OTLP complements rather than replaces sandboxing or local history. [OpenAI: approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security)

**Inference for Arany.** Adopt Codex's output-channel split, terminal usage event, explicit worker attribution, and fail-closed unattended approval behavior. Adapt thread switching into stable inline rows and causal detail backed by replay. Reject alternate-screen transcript ownership and any future bypass flag as beta precedents.

### Anthropic Claude Code

**Fact.** `claude -p` supports plain text, one-object JSON, and newline-delimited `stream-json`. Its final stream record contains the final response, cost, and session metadata. Streaming subagent messages carry `parent_tool_use_id`, allowing a consumer to associate work with the call that spawned it; optional forwarding exposes deeper subagent text. Retry events report attempt, maximum retries, delay, status, and error category. [Anthropic: programmatic/headless mode](https://code.claude.com/docs/en/headless)

**Fact.** Print mode exits zero on success and nonzero on failure. Invalid flags are reported on `stderr`, while failures inside an accepted run can be returned as the run result on `stdout`. It supports JSON Schema output, continuation/resume, explicit tool allowlists, deny-on-unattended permission behavior, and a recommended `--bare` mode that skips ambient hooks, skills, plugins, memory, MCP servers, and project instructions for more deterministic scripts. [Anthropic: programmatic/headless mode](https://code.claude.com/docs/en/headless)

**Fact.** Interactive mode distinguishes interruption from exit: `Ctrl+C` interrupts work, `Esc` stops the current response or tool call while retaining completed work, and a separate confirmed shortcut stops background subagents. A transcript viewer gives detailed tool activity without forcing all detail into the main view. [Anthropic: interactive mode](https://code.claude.com/docs/en/interactive-mode)

**Fact.** Experimental agent teams show named teammate rows, hide or collapse idle rows, permit transcript inspection and direct messages, and maintain a shared task list with pending, in-progress, completed, and dependency state. The docs also disclose important failures: task state can lag, shutdown can be slow, in-process teammates are not restored on resume, only one team exists per session, and nested teams are unsupported. [Anthropic: agent teams](https://code.claude.com/docs/en/agent-teams)

**Fact.** Claude exposes live context and spend signals, and documents that team cost scales with independently active contexts. Print-mode results can report estimated cost, and the CLI supports turn and dollar budgets. [Anthropic: costs](https://code.claude.com/docs/en/costs), [Anthropic: CLI reference](https://code.claude.com/docs/en/cli-reference)

**Inference for Arany.** Adopt causal worker attribution, explicit retry events, bounded execution, and a terminal result containing usage. Adapt teammate rows into one line per durable transition. Reject raw subagent thinking, split panes, peer mailboxes, mutable shared task lists, and any claim that a displayed task projection is authoritative.

### OpenCode

**Fact.** `opencode run` is the noninteractive entry point. It can continue, select, or fork a session and supports `--format json` for raw JSON events. Global logs are opt-in on `stderr`; `opencode stats` reports token and cost statistics; sessions can be listed as JSON and exported/imported. [OpenCode: CLI](https://opencode.ai/docs/cli/)

**Fact.** Subagent work creates child sessions. The TUI can navigate from a parent to its first child, cycle among siblings, and return to the parent. Built-in Explore and Scout workers are read-only. [OpenCode: agents](https://opencode.ai/docs/agents/)

**Fact.** OpenCode's server exposes session status, children, todos, diffs, abort, fork, revert, permission responses, and an SSE event stream. Its TUI is a client of that server, whose OpenAPI specification supports other clients. [OpenCode: server](https://opencode.ai/docs/server/)

**Fact.** Permission prompts offer approve once, approve matching patterns for the current session, or reject. Permissions are configurable per agent. [OpenCode: permissions](https://opencode.ai/docs/permissions/)

**Evidence gap.** The reviewed CLI page does not promise Codex's exact human-mode stdout/stderr split or a stable public exit-code taxonomy. Automation should therefore use the documented JSON event mode rather than scrape formatted output.

**Inference for Arany.** Adapt child-session identity, explicit abort, and status/event vocabulary to Engine-owned AgentRun Events. Reject the server/client topology, SSE, session import, revert, remembered pattern approvals, and TUI navigation: a one-process beta with SQLite replay needs none of them.

### Google Gemini CLI

**Fact.** Headless mode activates with `-p` or in a non-TTY. JSON output contains the final response, usage and latency statistics, and an optional error. Streaming JSON emits `init`, `message`, `tool_use`, `tool_result`, `error`, and terminal `result` records, with aggregate and per-model token statistics. Exit codes distinguish success, general/API failure, invalid input, and turn-limit exhaustion. [Google: headless mode](https://geminicli.com/docs/cli/headless/)

**Fact.** Gemini resumes by latest or session ID and can list sessions. Its session browser displays time, first prompt, and turn count. Rewind can independently restore conversation, AI-made file changes, or both; checkpointing stores the conversation and pending tool call alongside a shadow Git snapshot. [Google: CLI reference](https://geminicli.com/docs/cli/cli-reference/), [Google: session management](https://geminicli.com/docs/cli/tutorials/session-management/), [Google: rewind](https://geminicli.com/docs/cli/rewind/), [Google: checkpointing](https://geminicli.com/docs/cli/checkpointing/)

**Fact.** Subagents have independent context, bounded turns and timeouts, restricted tools, and a final handback to the main agent. They cannot recursively call other subagents. Policy rules can target a subagent, distinguish interactive from noninteractive runs, and choose allow, deny, or ask-user. [Google: subagents](https://geminicli.com/docs/core/subagents/), [Google: policy engine](https://geminicli.com/docs/reference/policy-engine/)

**Fact.** Optional telemetry includes agent start/finish, tool decisions, API errors/retries, token usage, chat compression, and approval-mode changes. It is an observability view rather than the documented session-history interface. [Google: telemetry](https://geminicli.com/docs/cli/telemetry/)

**Inference for Arany.** Adopt the small streaming-event taxonomy, distinct exit classes, worker bounds, and usage-bearing terminal result. Adapt session resume to deterministic `show` replay in beta. Reject checkpointing, rewind, mutable history, recursive teams, and telemetry as canonical state. Reconsider continuation only after recovery semantics exist for every event transition.

### goose

**Fact.** goose stores sessions in SQLite and supports session resume, fork, editable history, export, and JSON session lists. `goose run` has `text`, `json`, and `stream-json` output, `--quiet` to print only the model response, `--no-session` for ephemeral work, and explicit maximum-turn and repeated-tool-call bounds. [goose: CLI commands](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md)

**Fact.** In the CLI, each subagent tool call is printed inline with a subagent identifier, tool name, and extension, for example `[subagent:16] text_editor | developer`. Parallel delegation returns an execution summary and per-task statuses. The same official guide warns that a failed or timed-out subagent can return no output; parallel work then returns only successful results. [goose: subagents](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/context-engineering/subagents.mdx)

**Fact.** Headless mode uses preconfigured choices rather than live questions, automatically applies a context strategy, and is intended for CI, scheduled jobs, and batch work. The guide recommends checking exit status and bounding turns. [goose: headless mode](https://github.com/aaif-goose/goose/blob/main/documentation/docs/tutorials/headless-goose.md)

**Fact.** goose offers autonomous, manual, risk-classified smart approval, and chat-only modes; autonomous is documented as the default. The guide describes the read/write classification as a best-effort interpretation associated with the model/provider. [goose: permissions](https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/managing-tools/goose-permissions.md)

**Inference for Arany.** Adopt concise inline worker attribution, aggregate plus per-worker completion reporting, SQLite-backed history, and explicit bounds. Reject silent worker loss: every worker must end in a persisted success, failure, or cancellation event. Also reject autonomous-by-default authority and model-classified permission as security precedents.

### Aider

**Fact.** `aider --message` performs one instruction and exits. Scripting flags include streaming on/off, an input file, always-yes confirmation, automatic commits, and dry-run. The documented Python API is explicitly unsupported and unstable. [Aider: scripting](https://aider.chat/docs/scripting.html)

**Fact.** Aider's interactive commands include `/diff`, `/undo`, `/tokens`, `/clear`, `/drop`, `/test`, `/lint`, `/save`, and `/settings`. `Ctrl+C` safely interrupts a response and keeps the partial response in the conversation so the user can redirect the next turn. [Aider: in-chat commands](https://aider.chat/docs/usage/commands.html)

**Fact.** Aider can persist input, chat, and raw LLM histories and optionally restore chat history. It streams by default, can disable styled output, reports tool errors and warnings separately by presentation color, supports dry-run, and auto-commits model changes by default. [Aider: options](https://aider.chat/docs/config/options.html)

**Fact.** Its token-limit error reports input, output, and total counts and suggests concrete recovery actions such as dropping files, clearing history, or reducing change size. The counts are identified as estimates. [Aider: token-limit errors](https://aider.chat/docs/troubleshooting/token-limits.html)

**Evidence gap.** The reviewed official documentation does not define a JSON/JSONL lifecycle contract, stable event schema, worker attribution model, or multi-agent orchestrator view. Do not infer their absence from the entire product; they are simply not supported precedents for this report.

**Inference for Arany.** Adopt humane interruption, actionable limit errors, and explicit estimated-versus-authoritative usage wording. Adapt recovery commands to a replay hint because beta is read-only. Reject streaming model tokens, blanket yes-to-all approval, implicit auto-commit behavior, and unstructured output as Arany's automation contract.

## Cross-harness decision matrix

`Adopt` means the behavior fits the beta directly. `Adapt` means preserve the user contract while changing its presentation or implementation. `Reject` means it conflicts with the fixed beta or creates a premature seam.

| Pattern | Decision | Arany beta form |
|---|---|---|
| Final answer isolated from progress | **Adopt** | Interactive `arany`: sanitized primary result plus newline on `stdout`; committed progress and diagnostics on `stderr`. |
| Typed machine stream | **Adopt** | `--jsonl`: exactly one serialized persisted Event per `stdout` line in ascending sequence; diagnostic envelopes only on `stderr`. |
| Stable lifecycle states | **Adopt** | Started, running, waiting, completed, failed, and cancelled for the Run and every admitted AgentRun. No free-form state inference. |
| One accountable final owner | **Adopt** | Children return bounded results; only the primary produces the user-facing final result. |
| Child/source attribution | **Adopt** | Every child line and Event carries `agent_run_id`, role, and a short stable display label; primary wait lines identify the outstanding child count. |
| Usage and bounds in terminal outcome | **Adopt** | Terminal Run event reports provider calls used/limit, provider-reported token fields when available, elapsed time, and terminal reason. Never fabricate currency cost. |
| Fail-closed unattended behavior | **Adopt** | Beta has no effectful Tool or approval prompt. Any unavailable required input or future ungranted effect fails rather than waiting or widening authority. |
| Structured cancellation | **Adopt** | First `Ctrl+C` cancels the primary and every active child and persists cancellation; second signal or cleanup deadline forces abort, then returns 130. |
| Deterministic history and replay | **Adopt** | `show` reduces validated SQLite Events and renders deterministic human or JSONL output. |
| Actionable errors | **Adopt** | State the failing phase, stable symbolic code, affected run/worker when known, last durable sequence, and safe next command. Never expose secrets or raw provider bodies. |
| TUI agent rows, panes, spinners, todo widgets | **Adapt** | Use a bounded inline composer/footer and conditional activity shelf over native scrollback; no alternate screen, idle animation, or miniature IDE. |
| Full child transcript drill-down | **Adapt** | Preserve typed child Events for `/agents` and `show`; do not stream private reasoning or every token/tool payload into the primary transcript. |
| Resume and fork | **Adopt** | Beta uses durable explicit Session resume and committed-boundary fork; interrupted provider work is never silently continued. |
| Approval source labels | **Adapt** | Reserve actor and intent identity in the Event vocabulary. There is no approval UI until effectful Tools and the separate Guard exist. |
| Cost reporting | **Adapt** | Mark values as provider-reported, locally calculated, estimated, or unavailable. Calls, tokens, elapsed time, and configured limits remain the portable truth. |
| Retry feedback | **Adapt** | The beta Engine performs no provider retry. If retry is later added, emit attempt, cap, delay, cause class, and final exhaustion as Events. |
| Model-token streaming | **Reject** | It breaks deterministic replay, complicates cancellation, and weakens the clean final-result boundary without proving the team loop. |
| TTY-dependent bytes or rich terminal rendering | **Reject** | TTY and redirected output remain byte-identical; no ANSI, redraw, alternate screen, or OSC links. |
| Raw reasoning and unrestricted worker logs | **Reject** | Show authored progress summaries and typed outcomes, never private chain-of-thought or unbounded payloads. |
| Peer mailboxes, shared mutable task lists, nested teams | **Reject** | Fixed root-plus-two manager/worker fan-out and join only. |
| Daemon, server, SSE, remote client, or import/export protocol | **Reject** | One CLI process and in-process Engine calls only. |
| Remembered prefix approvals or blanket auto-approve | **Reject** | Future effects require typed immutable intents and deterministic Policy/Guard enforcement. |
| Rewind, checkpoint, auto-commit, and code undo | **Reject** | The first slice is read-only; SQLite Events are append-only and never rewritten. |
| Telemetry as user feedback or truth | **Reject** | OTLP ships last in beta, is runtime-opt-in and lossy. SQLite Events and SessionView/RunView remain canonical even when export fails. |

## Recommended human and machine contracts

Human output should remain deliberately plain:

```text
run 01J... started
root 01J... running
worker 01J.../a spawned: inspect persistence
worker 01J.../b spawned: inspect cancellation
root 01J... waiting: 2 workers
worker 01J.../a completed: persistence result ready
root 01J... waiting: 1 worker
worker 01J.../b failed: H_PROVIDER_UNAVAILABLE
root 01J... failed: worker 01J.../b failed
run 01J... failed: calls 3/4; replay with `arany show 01J...`
```

The labels are trusted Engine data; objectives, summaries, and errors are untrusted fields and must be terminal-sanitized and size-bounded. The root final answer is not repeated on `stderr`.

JSONL should expose the persisted envelope rather than a second ad hoc protocol. Consumers must be able to order records by Run sequence, correlate worker records to the root, identify a single terminal Run outcome, and detect a truncated stream. Human diagnostics in JSONL mode use a separate diagnostic schema on `stderr` and never masquerade as Events.

## Small original improvements for Arany

These improvements address gaps shared by the reviewed tools while staying inside the beta boundary.

### 1. Durable stream receipt

End every human run with the Run ID, terminal reason, and last committed sequence; put the same fields in the terminal JSONL Event. A consumer that saw a shorter sequence knows its pipe was truncated and can recover with `show`. None of the reviewed human-mode contracts makes the durable replay cursor this explicit.

### 2. Causal join feedback

Have primary wait Events name the exact outstanding child IDs and count. Completion/failure Events carry the assignment identity that satisfied or broke the join. This gives orchestrator-to-child visibility without a permanent dashboard or raw transcript and makes any admitted bounded topology easy to verify.

### 3. Three-part terminal accounting

The terminal Run summary should separate `outcome`, `usage`, and `recovery`: what happened; calls/tokens/time consumed; and the exact safe replay command. Existing products expose these fields across final messages, status widgets, and logs; Arany can make them one bounded durable projection.

### 4. Explicit partial-result ledger

On any failure or cancellation, record one terminal status for each worker before the Run ends, even if a worker produced no result. This directly avoids goose's documented silent failed-worker gap and prevents a successful sibling from disguising an incomplete join.

## Source-quality and scope limits

This is documentation research, not a black-box terminal test. Vendor output can change by release, and some documented surfaces are experimental. The recommendations depend only on behaviors stated in the cited first-party pages. They intentionally exclude unverified screenshots, blog comparisons, community anecdotes, and undocumented source behavior.

The strongest reusable idea is not a visual component. It is a contract: one durable ordered event truth, two deliberate renderers, explicit actor attribution, and one accountable terminal result.

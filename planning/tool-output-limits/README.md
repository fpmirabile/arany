# Tool output limits: truncate, page and clear like the peers

## Metadata

- Status: `In progress`
- Owner: Fernando Mirabile
- Last updated: 2026-10-20

## Outcome and scope

The owner's first real coding session stopped short. Grouped reads through a Command failed as `limit` with no output, a Read came back as 45 lines, and a missing path was reported as a boundary refusal. The agent then gave up: "the grouped reads via commands hit the Tool result limit … No files modified." This plan makes the Tool bounds behave like the peers' bounds. An oversized result truncates with a marker. A Read pages by lines and never shrinks because the Run is filling up. Old results are cleared before the Run runs out of room. A missing path says so. Every bound stays declared ([global rule 4](../../AGENTS.md#global-rules)). Only the behavior at the bound changes, from failing the whole record to a marked, bounded excerpt.

This serves agent execution and context ([priority 1](../../AGENTS.md#product-and-implementation-priorities)). Each choice follows [priority 4](../../AGENTS.md#product-and-implementation-priorities): the [decisions](#decisions) cite the peer behavior, and a divergence remains only where a [guarantee](../../AGENTS.md#non-negotiable-guarantees) requires it.

Out of scope:

- Raising the Write/Edit content cap and the 1 MiB Edit file cap ([Daily use item 5](../../NEXT_STEPS.md#daily-use-first) owns them). D12 records why they do not belong here.
- Command snapshot and changeset caps ([Daily use item 8](../../NEXT_STEPS.md#daily-use-first)).
- Regex Search, the 64-row List/Search cap, Glob and parallel reads ([Daily use item 11](../../NEXT_STEPS.md#daily-use-first)).
- Generic Provider retries ([engine loop R8](../engine-loop/README.md#r8-bounded-journaled-provider-retries-one-author)).

## Reported failures and evidence

Verified on `main` at 8d9cdd81. The transcript is `harness-team/logs/owner-dogfood-1.txt` (outside the repository). The owner's journal for that Session was not available: the local store holds only an earlier Session from 2026-10-19, which supplies the measured budgets below.

| # | Owner report | Cause in code |
|---|---|---|
| L1 | `Command(sh -c for f in agents/testing.md …)` returned `limit: Tool result or resource limit reached · cache not attempted`, three times. | The Engine sizes each result as `min(64 KiB, (remaining Tool context − overhead − 512 − reserves) / 6)` (`engine/run_loop/step.rs:478-494`, `tools.rs:477-483`). The ÷6 reserves worst-case JSON escaping. In the 2026-10-19 journal, the first Tool of a fresh Run got `result_bytes` between 46,794 and 47,304, not 64 KiB. A Command then captures each stream with a cap of `result_bytes / 2 − 128` (`tools/process.rs:103-106`), about 23.5 KB. The six files named in the first Command alone total 45,087 bytes. `read_capped` fails as soon as a stream passes its cap (`tools/guard.rs:789-790`). `capture` then kills the process (`guard.rs:1216-1222`), and `dispatch::outcome` replaces everything with one fixed string (`tools/dispatch.rs:65-68`). No output reaches the model, and no changeset is computed (`process.rs:122-131`). |
| L2 | `· cache not attempted` on each `limit` row; the owner asked "not sure if it's only cache or reading". | A cache-backed Guard Command that ends without publication records `CacheUpdate::NotAttempted` (`guard.rs:1539-1545`). The row appends every cache label (`arany-cli/src/presentation.rs:161-169, 436-447`). [Tools](../../docs/specs/tools.md#persistent-workspace-build-cache) says text receipts surface only rejected, failed and uncertain updates. The row diverges from the spec. |
| L3 | `Read(agents/testing.md)` → `45 lines (truncated)`. The file has 80 lines and 11,491 bytes. | Read pages by bytes: `offset` and `limit` are UTF-8 byte counts up to 65,536 (`tools/fs.rs:55-64`). The page stops at the smaller of the model's `limit` and the fitted result budget (`fs.rs:944-982`). That budget is the Run's remaining Tool context ÷ 6 (L1), so it shrinks as the Run fills. The Tool allowance is what is left of the fixed 376 KiB compiled-context ceiling (`session.rs:14`) after the objective, instructions, includes, history, child reserve and Goal metadata (`session/context.rs:185-193`). A file over 1 MiB cannot be read at all (`fs.rs:997`, `MAX_FILE_BYTES`). |
| L4 | `Read(agents/skills/ponytail/SKILL.md)` → `denied: File path is unavailable or outside the admitted regular-file boundary`. The file does not exist; the Skill lives under `.agents/skills/`. | `open_file` maps every `openat2` error, including `ENOENT`, to `ToolError::Path` (`fs.rs:339-356`). `dispatch::outcome` maps `Path` to `Denied` with the boundary text (`dispatch.rs:73-76`). The macOS branch of `open_file` does the same. |
| L5 | The `limit` wording does not say which limit. | One string covers result overflow, a Command timeout (`guard.rs:1219`), an in-process read timeout (`tools.rs:1161`) and a full context (`step.rs:271-273`). The row prints `limit: ` plus that text (`presentation.rs:144-158`). |

Other paths with the same whole-record failure:

- An MCP result over 64 KiB is `Limit` (`tools/mcp.rs:230, 257`).
- Unguarded Command output over the bound is `Limit`, "host effects may have occurred" (`tools/unguarded.rs:158-175`).
- Command output that is not valid UTF-8 fails as `Operation` (`process.rs:123-124`).

Typed Git reads already keep 256 KiB, cut the rest and mark `truncated` ([Tools](../../docs/specs/tools.md#limits-and-failure-behavior)). This plan generalizes that existing precedent.

### Corrections to the diagnosis

- (1) holds, but the 64 KiB ceiling is never reached. The ÷6 escaping reserve gives about 46 KiB per result at Run start. A Command gets half of that per stream, minus 128 bytes. Overflow also kills the process mid-run.
- (2) holds in part. The 376 KiB is the Run's whole compiled-context ceiling, and Tool observations get what remains. In-Run compaction already exists: it summarizes old observations through a charged Provider call at two thirds of the allowance (`engine/run_loop/tool_compaction.rs:160`). What is missing is cheap clearing before summarizing, and a per-call budget that does not shrink. Whether the model's byte `limit` or the fitted budget produced the 45-line page is not recorded in any available journal ([Q2](#open-questions)). Slices 2 and 3 remove both causes.
- (3) and (4) hold as stated. The cache label in (4) also contradicts its own spec (L2).

## Peer facts

All peer sources were checked on 2026-10-20. Codex, OpenCode and Gemini CLI links are pinned to the commits read.

| Peer | Fact | Source |
|---|---|---|
| Claude Code | Bash output is inline up to about 30,000 characters (`BASH_MAX_OUTPUT_LENGTH`, at most 150,000). A larger valid result becomes a file path in the session directory plus a preview of the first 2,000 characters. A failed command gets a head-and-tail excerpt of about 10,000 characters, with no file. A command whose output passes 5 GB is killed. | [Tools reference: output limits](https://code.claude.com/docs/en/tools-reference#output-limits), [environment variables](https://code.claude.com/docs/en/env-vars) |
| Claude Code | Read returns at most 25,000 tokens per call (`CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS`). It pages larger files with `offset` and `limit`. A whole file over the token limit returns its first page with a `PARTIAL view` notice. A whole-file read over 256 KB, or a page over the token limit, is an error that suggests paging or Grep. The shipped Read tool description sets a 2,000-line default. A 2,000-character line cap is widely reported but not documented; it is not adopted from Claude Code. | [Tools reference: large files](https://code.claude.com/docs/en/tools-reference#large-files), [issue #6910](https://github.com/anthropics/claude-code/issues/6910) |
| Claude Code | MCP responses are capped at 25,000 tokens (`MAX_MCP_OUTPUT_TOKENS`). A successful text result over 50,000 characters is saved to a file. | [Environment variables](https://code.claude.com/docs/en/env-vars) |
| Claude Code | "It clears older tool outputs first, then summarizes the conversation if needed." By default auto-compaction runs at the model's context limit. The API's `clear_tool_uses_20250919` strategy triggers at 100,000 input tokens by default, keeps the 3 most recent tool uses and replaces each cleared result with a placeholder. | [How Claude Code works](https://code.claude.com/docs/en/how-claude-code-works#when-context-fills-up), [model configuration](https://code.claude.com/docs/en/model-config), [context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing) |
| Codex CLI | Each Tool output is truncated by the model's `truncation_policy`: 10,000 tokens in the bundled catalog, or 10,000 bytes in the fallback metadata, at about 4 bytes per token. Truncation keeps an even head and tail around `…N tokens truncated…` (or `…N chars truncated…`). The result is prefixed `Warning: truncated output (original token count: N)` and `Total output lines: M`. Output is decoded lossily. | [output-truncation/src/lib.rs:23-42](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/utils/output-truncation/src/lib.rs#L23-L42), [string/src/truncate.rs:6, 36-42, 142-153](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/utils/string/src/truncate.rs#L36-L42), [models.json:1340-1371](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/models-manager/models.json#L1340-L1371), [model_info.rs:126-132](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/models-manager/src/model_info.rs#L126-L132), [tools/context.rs:471](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/core/src/tools/context.rs#L471) |
| Codex CLI | A timed-out command returns `command timed out after N milliseconds` followed by its partial output, under the same truncation. | [core/src/tools/mod.rs:128-139](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/core/src/tools/mod.rs#L128-L139) |
| Codex CLI | Current source has no dedicated read Tool. Reads go through shell commands under the same truncation. Truncation applies to live history only; the rollout keeps the full payload. No per-turn aggregate Tool budget was found. The aggregate bound is the context window (272,000 tokens in the bundled catalog), auto-compacted at 90% of it. | [context_manager/history.rs:518-576](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/core/src/context_manager/history.rs#L518-L576), [openai_models.rs:529-540](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/protocol/src/openai_models.rs#L529-L540) |
| OpenCode | Tool output is capped at 2,000 lines or 50 KiB. The head is kept and the full output goes to a file kept for 7 days, marked `[showing lines 1-N of M; full output saved to FILE]`. | [tool-output.ts:13-15, 86-90](https://github.com/anomalyco/opencode/blob/133b1ebfafc3ae040b9b23ff3fcad49e273c9cf9/packages/core/src/tool-output.ts#L13-L90) |
| OpenCode | Read defaults to and caps at 2,000 lines and 50 KiB. Lines longer than 2,000 characters are cut with `... (line truncated to 2000 chars)`. Lines are numbered `N: content`, and a cut page ends with `[Output truncated. Continue reading with offset: N]`. A missing path gives `File not found: PATH` with up to three suggestions. | [read-filesystem.ts:14-20](https://github.com/anomalyco/opencode/blob/133b1ebfafc3ae040b9b23ff3fcad49e273c9cf9/packages/core/src/tool/read-filesystem.ts#L14-L20), [read.ts:152-212](https://github.com/anomalyco/opencode/blob/133b1ebfafc3ae040b9b23ff3fcad49e273c9cf9/packages/core/src/tool/plugin/read.ts#L152-L212) |
| OpenCode | Automatic compaction leaves `max(15% of the window, 16,000 tokens)` free. An unknown window is assumed to be 200,000 tokens. | [session/compaction.ts:93-103, 845-855](https://github.com/anomalyco/opencode/blob/133b1ebfafc3ae040b9b23ff3fcad49e273c9cf9/packages/core/src/session/compaction.ts#L93-L103) |
| Gemini CLI | Shell output over `truncateToolOutputThreshold` (default 40,000 characters) keeps the first 20% and the last 80% around `... [N characters omitted] ...`. The full output is saved to a file. | [settings.schema.json:3131-3136](https://github.com/google-gemini/gemini-cli/blob/76e4d7c046c7dab5727787da378b9c06bf0948cf/schemas/settings.schema.json#L3131-L3136), [fileUtils.ts:786-806](https://github.com/google-gemini/gemini-cli/blob/76e4d7c046c7dab5727787da378b9c06bf0948cf/packages/core/src/utils/fileUtils.ts#L786-L806) |
| Anthropic API | The Models API reports `max_input_tokens` for each model. | [Get a Model](https://platform.claude.com/docs/en/api/models/retrieve) |

Every peer truncates at its per-call bound instead of failing. Per-call sizes cluster at 30-50 KiB of text, so Arany's 64 KiB ceiling is already peer-sized. What differs is the behavior at the bound and the ÷6 shrink.

## Decisions

Each row is **decided: peer parity** unless marked otherwise.

| # | Decision | Peer citation | Guarantee note |
|---|---|---|---|
| D1 | Command, unguarded Command, worker-view Command and MCP results that exceed the result budget truncate. They keep an even head and tail around an Engine-authored marker that gives omitted and total bytes and lines. Typed totals sit beside the text. | Codex: even head and tail with totals. Gemini: 20/80 head and tail. Claude Code: head and tail for failures. OpenCode: head only. The even split serves both compiler errors (first) and test summaries (last). | Journal-first: the truncated text and its totals are the committed observation. Replay reads them and never reruns the command. |
| D2 | No spill file for the full output. The model narrows with another command (`grep`, `sed -n`, `tail`). | Codex keeps no spill file. Claude Code, OpenCode and Gemini do ([Q1](#open-questions)). | A spill file would be a new private persistent format and a read surface outside the Workspace. It needs its own security pass. |
| D3 | Output is decoded as lossy UTF-8 (U+FFFD), and cuts fall on character boundaries. | Codex `from_utf8_lossy`. | Safe AI cage: the text stays untrusted data. |
| D4 | Output size no longer kills a process. Capture drains to end of file, keeps the windows and counts the rest. A declared drain ceiling of 1 GiB per stream kills the process and records `Limit` with an `output_ceiling` cause. | Claude Code kills past 5 GB. | Explicit bounds: memory is bounded by the windows and drain by the ceiling and the existing timeout. |
| D5 | The per-call result budget is the fixed 64 KiB of rendered (model-facing serialized) bytes. The Run's fill no longer narrows it. Every producer fits its rendered size, so the ÷6 reserve goes. `ToolObservation::valid` and the `ToolFinished` bound stay as the backstop. | Codex: fixed per-model policy. Claude Code: fixed 30,000 characters or 25,000 tokens. Neither shrinks a call because the conversation is long. | Explicit bounds: the ceiling is unchanged. JSON text re-escapes to at most twice its size, which the 144 KiB `ToolFinished` bound already admits. |
| D6 | Read pages by lines. `offset` is the 1-based start line (default 1). `limit` is a line count (default and cap 2,000). Lines over 2,000 characters are cut with a marker. Text is numbered `N\t` like `cat -n`. The page also stops at the rendered budget. The result keeps the file's `sha256` for Edit and returns `total_lines` and `next_line`. Read pages files up to 32 MiB, streaming the digest. | Claude Code: 2,000-line default and line-numbered output. OpenCode: 2,000 lines, 2,000-character lines, `N: ` numbering. Claude Code pages files of any size. | Isolation: the same admitted handle, no-follow rules and scope. 32 MiB matches the project-snapshot ceiling. |
| D7 | When the Run's Tool context nears its allowance, the Engine first clears old Tool results. It keeps the 3 most recent and replaces older bodies with a fixed placeholder. The call, disposition and effect fact stay. Summary compaction runs only if clearing frees less than one full result budget. Clearing is journaled as its own Event. | Claude Code clears older tool outputs first, then summarizes. Anthropic `clear_tool_uses` keeps 3. | Journal-first and strict replay: the clearing boundary is a committed Event, so a resumed Run renders the same bytes. Honest uncertainty: an Uncertain result keeps `uncertain; never replay` when its body clears. **Divergence:** the peers clear without a journal entry. |
| D8 | With trusted route window metadata, the Tool-context allowance derives from the window: `min(1 MiB, 0.9 × window tokens × 3 bytes)`. A reported input-token count at 90% of the window clears or compacts before the next call. Trusted metadata is the ChatGPT catalog's `context_window`. The Anthropic Models API's `max_input_tokens` is deferred to the next native-evidence schema change (slice 4). Without metadata the current 376 KiB and 512 KiB request ceilings stay. | Codex decodes `context_window` from the `models` reply into its model metadata ([endpoint/models.rs:70-80](https://github.com/openai/codex/blob/e8aa7c1678caa9a368243d72bf0e9b7859d97f15/codex-rs/codex-api/src/endpoint/models.rs#L70-L80)) and auto-compacts at 90%. Claude Code and OpenCode compact near the model's window. | **Divergence:** no assumed window for unknown routes (OpenCode assumes 200,000 tokens). The [Provider contract](../../docs/specs/provider.md#per-role-selection-and-fallback) admits no unverified window. The 3 bytes per token is more conservative than Codex's 4, and the reported-token trigger catches dense text. |
| D9 | A missing path is an attested `Failed` result: `File or directory not found; List the parent directory to check the path`. A missing parent for Write or Mkdir gets its own text. Link, mount, special-file and protected-name refusals keep the boundary text. | OpenCode `File not found`; shell `No such file or directory`. OpenCode's suggestions are skipped (Ponytail). | Safe AI cage: "not found" is reported only for a valid, in-scope, non-protected path, so it discloses nothing List does not. |
| D10 | `Limit` observations carry a closed cause: `timeout`, `output_ceiling`, `context_full`, `read_capacity`, `requested_time` or `resource`. A timed-out command returns its truncated partial output. | Codex: `command timed out after N milliseconds` plus output. | Changesets of timed-out commands still publish nothing ([Tools](../../docs/specs/tools.md#command-changesets)). |
| D11 | Rows show cache markers only for `rejected`, `failed` and `uncertain`. | Restores [Tools](../../docs/specs/tools.md#persistent-workspace-build-cache): "Text receipts surface rejected, failed and uncertain updates". | Uncertainty stays visible. |
| D12 | The 8 KiB Write/Edit content cap and the 1 MiB Edit input cap stay out of this plan. | n/a | Raising them changes input bounds that ripple elsewhere: the 16 KiB argument ceiling (`tools/types.rs:13`), the 64 KiB default `ToolStarted` payload ([persistence bounds](../../docs/specs/execution.md#persistence-bounds)) and `MAX_TOOL_OBSERVATION_BYTES`, which includes the argument ceiling (`types.rs:17-18`). Daily use item 5 owns that work. |

## Contract and scenarios

Owning specs:

- [Tools](../../docs/specs/tools.md): [limits](../../docs/specs/tools.md#limits-and-failure-behavior), [in-process reads](../../docs/specs/tools.md#in-process-reads), [Commands and build cache](../../docs/specs/tools.md#persistent-workspace-build-cache) and [Local MCP](../../docs/specs/tools.md#local-mcp).
- [Session](../../docs/specs/session.md#context-and-compaction): clearing and the window allowance.
- [Execution](../../docs/specs/execution.md#admission-persistence-and-recovery): payload versions and the in-Goal compaction paragraph.
- [Provider](../../docs/specs/provider.md#per-role-selection-and-fallback) and [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs): window metadata.
- [Terminal](../../docs/specs/terminal.md#conversation-progress-and-history): rows.

Each slice edits only the sentences it changes. Sentences that go:

- "Oversized requests and results otherwise reject whole records. This is a finite base, not automatic truncation of arbitrary files or unlimited model context" (tools.md:607). Oversized requests still reject whole; results truncate.
- "result ceiling 64 KiB, narrowed to remaining serialized space" (tools.md:604).

Rules for every slice:

- A change to Tool schemas, descriptions or request rendering bumps the native, custom, ChatGPT model-check and account-admission versions in the same PR, as the [tools-protocol plan](../tools-protocol/README.md#contract-and-scenarios) lists.
- A payload change follows "one current payload version" with no compatibility reader. It updates `session/events.rs::payload_version`, the [payload table](../../docs/specs/execution.md#admission-persistence-and-recovery) and the `events/tests.rs` assertions.

Scenarios. Each is pinned by a test in its slice:

- **Large command.** A `sh -c` that prints 1 MiB exits 0 and records `Succeeded`. Its stdout holds the head, the marker and the tail, with totals of 1,048,576 bytes. Its changeset publishes through the ordinary approval. The next step sees the marker.
- **Grouped reads (L1).** The transcript's `for f in …; do cat "$f"; done` over the six named files succeeds in one call.
- **Runaway output.** `yes` stops at the 1 GiB drain ceiling with `Limit(output_ceiling)` and publishes nothing.
- **Invalid UTF-8.** It renders as U+FFFD instead of failing.
- **Timeout.** A command that times out records `Limit(timeout)` with its partial output and publishes nothing.
- **Oversized MCP result.** It becomes a bounded text preview that keeps `is_error`.
- **Paging (L3).** `Read(agents/testing.md)` after twenty earlier steps returns all 80 lines. A 10,000-line file returns lines 1-2,000 with `next_line: 2001`, and the second call continues from there. A 3 MiB single-line file returns a cut line with a marker, not `Limit`.
- **Long Run.** Thirty sequential 40 KiB Reads in one Run each get a full page. Older results render as cleared, `ToolResultsCleared` Events are in the journal, and the Run does not stop with `ContextFull`. A crash after a clearing Event and before the next request resumes with the identical rendered request. When clearing cannot free one full budget, summary compaction runs (charged). If that cannot either, the Run stops with the existing `ContextFull` cause.
- **Window metadata.** A ChatGPT route whose catalog reports `context_window: 272000` pins it on `RunStarted` and gets a 734,400-byte allowance (`0.9 × 272,000 × 3`). A custom route keeps 376 KiB. An out-of-range catalog value is treated as absent.
- **Missing path (L4).** `Read(agents/skills/ponytail/SKILL.md)` records attested `Failed` with the not-found text, and the Run continues. A symlinked path, `.env` and a mount crossing still record `Denied` with the boundary text.
- **Rows (L2, L5).** A cache-backed command shows no `cache not attempted`. A timeout row reads `⎿ limit: timed out after 120 s; /settings raises it`.

Evidence owners:

- `crates/arany/src/tools/tests.rs`, `tools/fs.rs` and `tools/guard.rs` unit tests.
- `crates/arany/tests/engine_run.rs` corpora.
- `crates/arany-cli/tests/guard_runs.rs` journeys on Linux and macOS CI.
- `session/reducer/tests.rs` and `session/events/tests.rs`.
- `presentation.rs` snapshots.

Live and native checks go to [NEXT_STEPS](../../NEXT_STEPS.md) until stable pre-beta.

## Execution

Slices are ordered by owner impact. Each is one PR, and stacked PRs are fine ([Git](../../agents/git.md#pull-requests)). Slices 5 and 6 are small and independent, so they may land at any point. All slices apply [Ponytail](../../.agents/skills/ponytail/SKILL.md), [rust-clean-code](../../agents/skills/rust-clean-code/SKILL.md) and [clean-architecture-review](../../agents/skills/clean-architecture-review/SKILL.md).

### Slice 1: Command and MCP output truncate instead of failing

- **Scope.**
  - `guard::capture` keeps, per stream, a head buffer and a tail ring sized from the stream's share of the result budget. It drains to end of file, counts bytes and newlines, and kills only at the 1 GiB drain ceiling (D4).
  - `process::run` assembles stdout and stderr with lossy decoding (D3). It fits the changeset first and leaves it uncut: an over-limit changeset still rejects whole. Then it fits the exit code. The remaining rendered space is split evenly between the streams, and a short stream gives its unused share to the other (D1).
  - The marker is `\n…[N bytes, L lines omitted; T bytes, M lines total]…\n`.
  - `CommandResult` and `WorkerCommandResult` gain required `stdout_total` and `stderr_total` objects `{bytes, lines}`.
  - `unguarded::finish` applies the same rule.
  - `mcp.rs` replaces an oversized result with a head-and-tail text preview that keeps `is_error` (Codex `truncate_mcp_tool_result`).
  - The Command Tool description says that large output is truncated and how to narrow it.
- **Payloads.** `ToolFinished` 16 → 17. Replay checks that each total is at least the bytes shown.
- **Guarantees.**
  - Journal-first: the excerpt and totals are the committed observation, and strict replay never reruns.
  - Honest uncertainty: an Uncertain observation keeps its disposition.
  - Isolation: the changeset path is unchanged, and output size was never an authorization signal. A command whose output overflowed now publishes its changeset through the same approval and Guard integration.
  - The cage: the marker is Engine-authored, and the typed totals, not the text, are authoritative. Command output that imitates a marker stays data.
- **Specs.**
  - tools.md: limits table (a new "Command and MCP output" row; the tools.md:607 sentence); build-cache paragraph ("bounded UTF-8 output"; remove "overflowed" from the never-publish list; a drain-ceiling kill still never publishes); MCP row.
  - execution.md: payload table.
- **Tests.**
  - `guard.rs` unit: windows, marker, character boundaries, drain ceiling, and stderr flowing into stdout's share.
  - `process.rs` unit: changeset fitted first.
  - `guard_runs.rs`: the large-command, grouped-reads (L1 regression), runaway and invalid-UTF-8 scenarios on Linux and macOS.
  - `engine_run.rs`: the Run continues after a truncated command.
  - `mcp.rs` preview test.
  - Replay corpus.
- **OS impact.** `capture` and `process::run` are shared by the Linux helper and the macOS launcher path. Both CI lanes run the journeys, and macOS support needs that native evidence.

### Slice 2: line-paged Read with rendered-size budgets

- **Scope.**
  - `ToolCall::Read { path, offset, limit }` takes line semantics (D6). `offset` and `limit` become nullable integers: `offset` is 1-based and defaults to 1; `limit` defaults to and caps at 2,000.
  - `fs::read_page` streams lines from the admitted handle and the SHA-256 over the whole file (up to 32 MiB). It cuts lines over 2,000 characters with `… [line truncated to 2000 chars]`, numbers lines `N\t`, and stops at the line limit or the rendered budget.
  - The result is `{sha256, total_lines, start_line, end_line, next_line, truncated, text}`. `next_line` is null at end of file.
  - An offset past the end returns an empty page with `total_lines`, like Claude Code.
  - The Edit description says `old` must exclude the line-number prefix.
  - `sized_intent` drops the ÷6 reserve: `result_bytes = min(64 KiB, remaining − overhead − reserves)` (D5). List, Search, Read, Command and MCP already fit their rendered length, and Search rows keep their 512-character cut.
  - `provider/openai/wire.rs:205` validates the new ranges.
- **Payloads.** `ToolStarted` 12 → 13 for the Read call shape; `ToolFinished` +1. Check versions are bumped.
- **Guarantees.**
  - Isolation: the same pinned handle, recheck, no-follow, scope and protected-name rules. Streaming does not widen disclosure.
  - Journal-first: each page is a committed observation.
  - Bounds: 2,000 lines, 2,000 characters per line, 64 KiB rendered, 32 MiB input, and the existing read runtime.
- **Specs.**
  - tools.md: in-process reads (the result-fitting paragraph, tools.md:580); limits table "File operation" row (tools.md:591); the Tool-description sentence on Read paging.
  - execution.md: payload table.
- **Tests.**
  - `fs.rs` unit: default page, `next_line`, end of file, offset past the end, CRLF, empty file, long-line cut, 3 MiB single line, 20 MiB file, non-UTF-8 file.
  - `tools/tests.rs`: repeated-Read corpus.
  - `engine_run.rs`: Read of an 80-line file after twenty steps returns 80 lines (L3 regression).
  - Wire corpora: `openai/wire/tests.rs`, `openai/subscription/tests.rs`, `anthropic/wire/tests.rs`.
- **OS impact.** In-process reads are common code. The macOS `open_file` component walk is unchanged.

### Slice 3: fixed per-call budget; clear old results before summarizing

- **Scope.**
  - Before sizing a call, the Engine projects the context with one full 64 KiB result. If it exceeds the allowance, it clears first (D7). If clearing frees less than one budget, the existing summary compaction (`tool_compaction.rs`) runs, and then `ContextFull` stops honestly.
  - The existing two-thirds trigger also clears before it summarizes.
  - Clearing keeps the 3 most recent Tool results of the Run.
  - A cleared result renders as `{operation, call, disposition, effect, cleared: true}` with the fixed placeholder `Result cleared to save context; call the Tool again for current content`.
  - Never cleared: the invoked Skill, the plan, Goal facts, the pinned prefix and the Uncertain marker.
  - With clearing in place, every call gets the full 64 KiB, and slice 2's `min(remaining)` becomes a floor that always holds.
- **Payloads.** New `ToolResultsCleared {run_id, covered_tool_count, cleared_bytes}`, committed before the next Provider reservation. Replay requires a covered count that is monotone and on a whole-step boundary, and renders cleared bodies from it.
- **Guarantees.**
  - Journal-first: the rewrite boundary is committed first.
  - Strict replay: rendering is a pure function of committed Events, so a resumed request is byte-stable.
  - Honest uncertainty: Uncertain stays reported.
  - Durable Goals: clearing costs no Provider call, and summarizing stays charged.
  - The cage: cleared bodies never return except through a fresh Tool call.
- **Specs.**
  - session.md "Context and compaction": clearing as the first, journaled, deterministic rewrite. "Compaction is the only rewrite" becomes "clearing and compaction are the only rewrites".
  - execution.md: the in-Goal compaction paragraph (execution.md:199) and the payload table.
  - tools.md: the "Tool context" row (tools.md:604).
- **Tests.**
  - `engine_run.rs`: the long-Run scenario, crash between clearing and the request, and clearing that is not enough followed by summary then `ContextFull`.
  - `session/reducer/tests.rs`: the boundary rules.
  - `render/tests.rs`: the placeholder snapshot and a prefix that is byte-stable after clearing.
- **OS impact.** None.

### Slice 4: allowance from the selected route's window

- **Scope.**
  - The ChatGPT catalog row (`arany-cli/src/accounts/chatgpt/catalog.rs:26-30`) decodes an optional `context_window`. A value outside 32,768 to 2,000,000 tokens is treated as absent. The reply already has the shape of Codex's `models` reply (`slug`, `display_name`, `visibility`); [Q3](#open-questions) confirms that the endpoint Arany calls carries the field.
  - Anthropic `max_input_tokens` is deferred: native evidence has no slot for it without a SQLite schema change, and that change would force a history reset for an optional budget hint. [NEXT_STEPS](../../NEXT_STEPS.md#daily-use-first) item 13 wires it with the next native-evidence schema change.
  - OpenAI API, Anthropic and custom routes have no metadata.
  - `RunStarted.config` pins a nullable `context_window_tokens`.
  - `CompiledContext::compile` derives the content ceiling from it (D8). The request ceiling rises with it to `content + 256 KiB`, up to 1.25 MiB. Without metadata the values stay 376 KiB and 512 KiB.
  - After each Provider call, a reported input-token count at 90% of the window triggers slice 3 before the next call.
  - A context-length rejection fails that call as today. Engine loop R8 owns retries.
- **Payloads.** `RunStarted` 16 → 17. Check and account-admission versions are bumped.
- **Guarantees.**
  - The cage: the window comes only from the authenticated Provider catalog or Models API, never from repository content or model text. It can only resize budgets within declared hard ceilings, and it is pinned and journaled.
  - Durable Goals: larger requests spend the same pinned budgets.
- **Specs.**
  - provider.md: "Arany has no admitted context-window metadata" (provider.md:44).
  - chatgpt.md: "Catalog, checks and Runs".
  - execution.md:199: "A trusted exact route and model input-window bound may reduce the derived request budget" now covers raising it to the ceiling.
  - session.md: the `RunStarted` budget.
  - tools.md:604.
- **Tests.**
  - Catalog decode corpus: present, absent, out of range, duplicate.
  - `engine_run.rs`: the allowance math, the 90% trigger and a route without metadata.
  - Request-size refusal at the new ceiling.
  - Replay pin.
- **OS impact.** None.

### Slice 5: honest "not found"

- **Scope.**
  - `fs::open_file` maps `ENOENT` and `ENOTDIR` on Linux (`openat2`) and macOS (component `openat`) to a new `ToolError::NotFound`. Every other errno stays `Path`.
  - `dispatch::outcome` maps `NotFound` to attested `Failed` with the D9 text.
  - The Guard file worker maps a missing Write or Mkdir parent to `Parent directory not found; create it with Mkdir first`, and a missing Edit target to the D9 text.
- **Payloads.** None: `Failed` already exists with attestation.
- **Guarantees.**
  - The cage and isolation: path validation, protected names and scope run before `open_file`, so "not found" arises only inside the admitted scope. Links, mounts and special files keep `Denied`.
- **Specs.** tools.md: in-process reads (the failure text) and the file-operation paragraph ("Parent directories must already exist").
- **Tests.**
  - `fs.rs` unit per OS: missing file, missing parent, symlink, `.env`, special file.
  - `engine_run.rs` L4 regression: the Run continues after a missing path.
  - `guard_runs.rs`: Write into a missing parent on both OSes.
  - `presentation.rs` row.
- **OS impact.** Linux `openat2` and macOS `openat` errnos, covered by both CI lanes.

### Slice 6: named limits and quieter cache markers

- **Scope.**
  - `ToolObservation` gains a required nullable closed `limit_cause`, present exactly on `Limit` (D10).
  - `capture` returns its windows on timeout. The observation then carries the truncated partial output (D10), and the changeset still publishes nothing.
  - `cache_update_label` shows only rejected, failed and uncertain (D11).
  - Row copy follows the closed cause: `timed out after N s; /settings raises it`, `output passed 1 GiB; stopped`, `no room left in this Run's context`, `too many reads at once; nothing was read`, `asked for more time than the pinned maximum` and `resource limit reached`.
  - A truncated command row reads `exit 0 · 12,340 lines (truncated)`.
  - Load the `arany-terminal-design` skill.
- **Payloads.** `ToolFinished` +1, and the Guard observation frame changes with it.
- **Guarantees.** Uncertainty, enforcement-cause and denial markers stay mandatory.
- **Specs.**
  - terminal.md "Conversation, progress and history": the row results and "cache markers" narrowed to problem markers.
  - tools.md: the time row (partial output on timeout).
- **Tests.**
  - `presentation.rs` snapshots at 80 and 120 columns and linear mode.
  - `guard_runs.rs`: timeout with partial output on both OSes.
  - Reducer: `limit_cause` presence.
- **OS impact.** The macOS launcher reports the timeout. Its report must carry the bounded windows, so native macOS evidence is required before claiming parity.

## Security and resource model

[Security-pass triggers](../../agents/security.md#security-pass-triggers):

- Slice 1: child-process output.
- Slice 2: filesystem access.
- Slices 3-4: prompt assembly and Provider cost.
- Slice 5: path disclosure.

Assets are Workspace content, Provider spend and journal integrity. Attacker-controlled inputs are command and MCP output, file contents and Provider catalog metadata. Authority is unchanged: the pinned grant, permission mode, hooks and Guard. Only the Read call shape changes in the normalized intent.

Disclosure never widens:

- A truncated result shows less than the full output.
- "Not found" covers only paths that List could already enumerate.

Non-claims:

- Truncation is not redaction.
- Clearing does not erase canonical Events.
- Window metadata is not a Provider-side guarantee.

| Bound | Value | Overflow |
|---|---|---|
| Per-call result | 64 KiB rendered | head and tail around a marker, with typed totals |
| Command capture memory | head and tail windows per stream within the result budget | the middle is discarded and counted |
| Command drain | 1 GiB per stream; existing timeout | kill, `Limit(output_ceiling)` |
| Read page | 2,000 lines; 2,000 characters per line; 64 KiB rendered | `next_line`; line marker |
| Read input | 32 MiB regular file | `Limit` |
| Kept recent results | 3 per Run | older bodies cleared and journaled |
| Tool-context allowance | window-derived, at most 1 MiB; 376 KiB without metadata | clear, then summarize, then `ContextFull` |
| Provider request | allowance + 256 KiB, at most 1.25 MiB; 512 KiB without metadata | refused before egress |
| `ToolFinished` | unchanged: 144 KiB + 512 | append refused |

## Risks and mitigations

- **The middle hides the error.** The marker gives counts and line totals, and the Command description says to narrow with `grep`, `sed -n` or `tail`, the Codex practice. Q1 stays open for a spill file.
- **Clearing defeats prompt caching.** Clearing runs only at the threshold, keeps 3 results and clears in one batch, so the prefix is stable again until the next threshold. Cache-read usage is already journaled per call, so the effect is measurable.
- **Larger requests spend more plan quota.** Only routes with trusted metadata grow. Goal budgets are unchanged and visible.
- **The 3 bytes per token estimate is wrong for dense text.** The reported-token trigger at 90% bounds it: one step adds at most about 64 KiB, under the 10% margin. A remaining rejection fails honestly; engine loop R8 adds bounded retries.
- **The model copies line numbers into Edit `old`.** The Edit description says not to, Edit refusals are fixable, and the peers carry the same risk.
- **A changeset now publishes after an output overflow.** This is intended: the approval and Guard integration are unchanged, and a scenario pins it.
- **Version bumps invalidate stored journals.** Use the [explicit history reset](../../docs/specs/terminal.md#setup-and-account-choices), accepted pre-beta practice. The owner reruns `provider check`.

## Open questions

- **Q1 (owner, non-blocking).** Spill the full output to a private per-Run file that a Read can page, like Claude Code, OpenCode and Gemini? Recommended: not now. Codex does without one, and a spill is a new persistent format and read surface that needs its own security pass. The plan proceeds without it.
- **Q2 (owner, non-blocking).** Which produced the 45-line page, the model's byte `limit` or the fitted budget? `arany show` on the dogfood Session answers it if the journal still exists. Slices 2 and 3 remove both causes either way.
- **Q3 (owner, blocks slice 4 for ChatGPT only).** Does the catalog reply from `https://api.openai.com/v1/models` (`catalog.rs:9`) carry `context_window` for the owner's account? Codex's bundled catalog lists 272,000 tokens for current models, but this plan saw no live reply. If the field is absent, the ChatGPT route keeps the conservative bounds, and slice 4 changes no route's bounds until Anthropic metadata lands.

## Exit criteria

- All six slices merged with green CI. Each regression test reproduces its report (L1-L5).
- The specs listed per slice describe the current behavior. NEXT_STEPS records any unverified macOS live check.
- Live owner check on a ChatGPT-plan account, in attached chat, in this repository. Repeat the transcript's task: "work on planning/terminal-parity", from a new Session. Results go to NEXT_STEPS until stable pre-beta.
  1. A grouped `sh -c 'for f in …; do cat "$f"; done'` over the transcript's files shows one Command row with a line count, `(truncated)` if needed, and never `limit`.
  2. `Read(agents/testing.md)` shows all 80 lines.
  3. A Read of `agents/skills/ponytail/SKILL.md` shows the not-found text, and the agent finds `.agents/skills/ponytail/SKILL.md` without help.
  4. No row says `cache not attempted`.
  5. The agent reads what it needs and proceeds to edit and verify. It does not say it hit a Tool result limit.
  6. `arany show --output text` lists the truncated observations with totals and any `ToolResultsCleared`. A Run of 30 or more Tool calls ends without `ContextFull`.
- Closeout under the [planning lifecycle](../README.md#lifecycle): delete this plan when the last slice lands.

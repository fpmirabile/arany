# Route output budget covers the Write and Edit content limit

## Metadata

- Status: `Ready`
- Owner: Fernando Mirabile
- Last updated: 2026-10-23

## Outcome and scope

One feature: a Write, Edit or MultiEdit at the advertised 64 KiB content limit ([Tools limits](../../docs/specs/tools.md#limits-and-failure-behavior)) completes on every admitted route that can write, because the output-token cap of every write-capable call (primary, private editor) covers that content plus answer headroom, while the cap stays finite, declared and journaled. The harness responsibility is Tool execution under explicit resource bounds ([AGENTS.md rule 4](../../AGENTS.md#global-rules)).

Routes in scope: the ChatGPT-plan route and the native OpenAI and Anthropic API routes. Out of scope: custom routes (`custom:NAME`, `provider-profiles.json`, the `max_output_tokens: 4096` profile field) stay deferred until [Provider setup UI](../provider-setup-ui/README.md) lets users configure them from the terminal.

## Research

### Live defect (2026-10-23)

Asked to Read a 65,536-byte file and Write an identical copy on the ChatGPT route, the model declined twice ([NEXT_STEPS item 8](../../NEXT_STEPS.md#daily-use-first)). The arithmetic confirms it could never succeed: 64 KiB of source is roughly 16,000 to 22,000 argument tokens, and Tool-call arguments count as visible output, so the call would end in `local_output_limit` ([ChatGPT Run limits](../../docs/specs/chatgpt.md#run-limits)). The native Anthropic route showed `output_token_cap: 16384` on `RunStarted`, which is borderline for the same Write; the Anthropic adapter counts thinking tokens inside `usage.output_tokens` against that cap (`crates/arany/src/provider/anthropic/wire.rs:750`).

### Where the per-route output budget lives today (module map)

The cap is one number per route, but ten places know its value or its invariants:

| Place | What it knows | File |
|---|---|---|
| Provider constants | `MAX_OUTPUT_TOKEN_CAP = 16_384` (ceiling and native route request), `DEFAULT_OUTPUT_TOKEN_CAP = 4096` (`CallLimits::default`, ChatGPT, children) | `crates/arany/src/provider.rs:190-191,399-412` |
| Identity validity | `1..=MAX_OUTPUT_TOKEN_CAP` for every identity, live and on replay | `crates/arany/src/provider.rs:492` |
| Native OpenAI identity | requests `MAX_OUTPUT_TOKEN_CAP`; sends `max_output_tokens`; `usage.output_tokens <= cap` | `crates/arany/src/provider/openai.rs:148,213-219`, `openai/wire.rs:496` |
| Native Anthropic identity | requests `MAX_OUTPUT_TOKEN_CAP`; sends `max_tokens`; only `end_turn` and `tool_use` stops are admitted, so a `max_tokens` stop fails as `InvalidOutcome` | `crates/arany/src/provider/anthropic.rs:183`, `anthropic/wire.rs:442,735-750` |
| ChatGPT identity | `CallLimits::default()` (4,096) with `LocalAcceptanceOnly`; `invoke_at_observed` rejects a request above `DEFAULT_OUTPUT_TOKEN_CAP`; `accept_result` compares visible output with the request cap; `COMPACTION_VISIBLE_CAP` | `crates/arany/src/provider/openai/subscription.rs:33,63-67,305,398-437` |
| Route chain validity | a `LocalAcceptanceOnly` identity must declare `<= DEFAULT_OUTPUT_TOKEN_CAP` | `crates/arany/src/provider/routing.rs:188-190` |
| Engine admission | repeats the same `LocalAcceptanceOnly <= DEFAULT` invariant in `checked_identity` | `crates/arany/src/engine.rs:2528-2537` |
| Primary request | `max_output_tokens: identity.limits.max_output_tokens`, narrowed by `Routing::bind` to the bound route | `crates/arany/src/engine.rs:2469`, `engine/routing.rs:210-222`, `engine/run_loop.rs:797-802` |
| Children | `CHILD_OUTPUT_TOKEN_CAP = DEFAULT_OUTPUT_TOKEN_CAP` for researchers and private editors alike | `crates/arany/src/engine.rs:66`, `engine/run_loop/children.rs:2398` |
| Tool review, compaction | 256 and `MAX_COMPACTION_OUTPUT_TOKENS = 1024`, both narrowed by `bind` | `engine/run_loop/step.rs:2101`, `session/compaction.rs:21`, `engine/compaction.rs:423` |
| Journal | `RunStarted.config.output_token_cap` rebuilds the identity on replay and is validated through `ProviderIdentity::valid` | `crates/arany/src/session/events/records.rs:141,268-278` |
| Context reservation | `max_turn_bytes(cap) = 8 * cap + 64 KiB` (at most 1 MiB) feeds `continuation_reserve`; in-Run compaction halves a reserve above the Tool context limit | `provider/conversation.rs:192-194`, `tools.rs:547-558`, `engine.rs:2111-2125`, `engine/run_loop/tool_compaction.rs:183-191` |

Specs stating the values: [execution.md:151](../../docs/specs/execution.md) ("Native OpenAI and Anthropic API routes declare 16,384 output tokens, and ChatGPT-plan and exact custom routes 4,096 ... replay rejects a pinned cap outside 1 to 16,384. Child calls request at most 4,096 tokens."), [chatgpt.md:59](../../docs/specs/chatgpt.md#run-limits) (primary and child 4,096, Tool review 256), [provider.md:42](../../docs/specs/provider.md) (role-specific caps).

The Tool side has one owner: `MAX_EDIT_CONTENT_BYTES = 64 * 1024` (`crates/arany/src/tools/types.rs:14`), checked in `fs::valid` and printed into the model-facing schema text (`crates/arany/src/tools/fs.rs:36-71,150,165`), with the serialized-argument bound `MAX_TOOL_ARGUMENT_BYTES = 400 KiB` (`types.rs:17`). The schema text is static (`OperationFacts::schema` in `tools/catalog.rs:366`) and the catalog JSON is pinned per Run. PR #234 (`d7e4c3d2`) raised the content limit to 64 KiB and sized the argument, `ToolStarted` and Guard frames for it, but left every route cap untouched.

### Peer behavior

- Codex CLI (openai/codex d571eaf1, recorded in [chatgpt.md:65](../../docs/specs/chatgpt.md)): the Responses request carries no `max_output_tokens` and no client-side output cap exists.
- Claude Code (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, [environment variables reference](https://code.claude.com/docs/en/env-vars), read 2026-10-23): "Set the maximum number of output tokens for most requests. Defaults and caps vary by model ... Claude Code lowers a value above a model's cap to the cap. For a model ID Claude Code can't resolve to a model it knows, the default is 32000 and the cap is 128000." The Write Tool has no content cap ([tools reference](https://code.claude.com/docs/en/tools-reference#write-tool-behavior), already cited in tools.md:916).
- Shared answer: neither peer bounds a file write below the model's output budget; the budget is large (32,000 default) or absent. Arany copies the large budget and keeps it finite ([AGENTS.md rule 4](../../AGENTS.md#global-rules); [D-18](../../docs/research/next-step-decision-register.md#d-18--explicitly-accepted-chatgpt-plan-route-is-beta-scope) accepts that the ChatGPT cap is local acceptance only).
- Model ceilings: every current Claude model allows 128K output tokens on the synchronous Messages API ([models overview](https://platform.claude.com/docs/en/about-claude/models/overview), read 2026-10-23); the native OpenAI route already sends `max_output_tokens` and admitted models accept 16,384 today.

### Sizing

Source text tokenizes at roughly 3 to 4 bytes per token; JSON escaping of the `content` argument adds a few percent. A conservative bound is `MAX_EDIT_CONTENT_BYTES / 3 = 21,846` tokens for the arguments plus the ordinary 4,096-token answer allowance, so 25,942 tokens. The next power of two, 32,768, covers it with headroom for Anthropic thinking tokens and matches Claude Code's 32,000 default. The escape-heavy worst case (six bytes per content byte, pinned by `escape_heavy_write_and_edit_decode_at_the_content_ceiling` at a synthetic 128,000 cap) needs more than 64,000 tokens on any tokenizer and is not a target: it fails honestly at the cap, as today.

## Contract and scenarios

- Owning specs and proposed changes: [execution.md "Provider calls"](../../docs/specs/execution.md) (route caps, child caps, replay range), [chatgpt.md "Run limits"](../../docs/specs/chatgpt.md#run-limits) (local cap values per role), [tools.md "Limits and failure behavior"](../../docs/specs/tools.md#limits-and-failure-behavior) (one sentence linking the content limit to the covering output cap). [provider.md:42](../../docs/specs/provider.md) keeps "role-specific output caps" and needs no edit.
- Successful path: primary on ChatGPT, native OpenAI or native Anthropic emits one Write with 65,536 content bytes; the call completes under the 32,768 cap, `ToolStarted` admits it, the Guard publishes it, `ToolFinished` records 65,536 bytes.
- Rejection: content of 65,537 bytes is refused whole by `fs::valid` with the schema's stated limit, unchanged ([tools.md:916](../../docs/specs/tools.md)); the model receives the refusal and reports it, never a truncated file.
- Failure: output beyond the cap stays an honest call failure with no partial outcome: ChatGPT `local_output_limit` without retry, native OpenAI `incomplete_details` as a response-contract failure (`openai/wire.rs:727`), native Anthropic `max_tokens` stop as `InvalidOutcome` (`anthropic/wire.rs:735-738`). Nothing truncates silently.
- Recovery and replay: `RunStarted` pins the new cap; strict replay re-validates it through `ProviderIdentity::valid` with the widened range; existing journals pin 4,096 or 16,384 and stay valid.
- Evidence owners: `crates/arany/src/provider/openai/subscription/tests.rs::the_local_cap_counts_visible_output_and_keeps_the_reasoning_total_for_charging`, `crates/arany/tests/engine_run.rs::provider_declared_output_cap_reaches_the_primary_and_caps_children` and `::subscription_account_and_local_bound_survive_run_and_compaction_replay`, `crates/arany/src/provider/openai/wire/tests.rs` and `anthropic/wire/tests.rs` content-ceiling rows, `crates/arany/src/provider/conversation.rs::turn_bound_follows_max_output_tokens`. Live copy check: owner, below.

## Design it twice

Constraints any design must satisfy: a finite declared cap per call, journaled on `RunStarted` and re-validated on replay; honest failure on overflow; the model told a true limit; no change to Guard, changeset or `ToolStarted` frames (sized for 64 KiB by #234). Dependencies are in-process (constants, validation) plus the Provider adapters at the existing external seam, already covered by the scripted Provider double; no new adapter is needed.

### Design A: write-capable calls request a cap that covers the Tool limit

Interface (one fact, one owner):

```rust
// crates/arany/src/provider.rs
pub(crate) const MAX_OUTPUT_TOKEN_CAP: u32 = 32_768;   // native routes, ChatGPT primary, editors; replay ceiling
pub(crate) const DEFAULT_OUTPUT_TOKEN_CAP: u32 = 4096; // read-only researchers, scripted default
// invariant pinned by a test: MAX_EDIT_CONTENT_BYTES.div_ceil(3) + DEFAULT_OUTPUT_TOKEN_CAP <= MAX_OUTPUT_TOKEN_CAP
```

Callers: `openai.rs`, `anthropic.rs` and `subscription.rs` identities declare `MAX_OUTPUT_TOKEN_CAP`; `children.rs` requests `MAX_OUTPUT_TOKEN_CAP` for an editor and `DEFAULT_OUTPUT_TOKEN_CAP` for a researcher, and `Routing::bind` still narrows to the route. The three copies of "LocalAcceptanceOnly implies `<= 4096`" (`routing.rs:190`, `engine.rs:2533`, `subscription.rs:305`) are deleted; `ProviderIdentity::valid` remains the single range check and `subscription.rs` rejects a request above its identity cap like the native adapters do.

Hidden behind the seam: visible-output accounting, wire field names, stop-reason admission, the opaque turn bound and replay validation, all unchanged. The Tool module's interface does not change: the schema text stays `at most 65536 bytes` and is now true on every route.

Trade-offs: highest leverage per line (one constant, one invariant, three deletions). Cost ceiling per call rises from 4,096 to 32,768 visible tokens on ChatGPT and from 16,384 on native routes; Goal token budgets, the 600-second deadline and the 1 MiB response ceiling still bound a call. `continuation_reserve` grows with the cap (320 KiB turn bound instead of 192 KiB), so in-Run Tool compaction can trigger sooner on small windows.

### Design B: the Tool content limit follows the route cap and the model is told

Interface:

```rust
// crates/arany/src/tools.rs
impl ToolRuntime {
    pub(crate) fn content_limit(output_token_cap: u32) -> usize;      // min(64 KiB, (cap - 4096) * 3)
}
// crates/arany/src/tools/fs.rs
pub(super) fn valid(call: &ToolCall, content_limit: usize) -> bool;
pub(super) fn schema(operation: &str, content_limit: usize) -> Option<Vec<(&'static str, Value)>>;
```

The Engine computes `content_limit` from the minimum cap over the primary chain (a fallback route may have a lower cap), renders it into the per-Run catalog text, and the reducer re-derives it from `RunStarted.output_token_cap` so replay validates `ToolStarted` identically. ChatGPT would advertise about 12 KiB, native routes about 36 KiB.

Trade-offs: truthful on every route without touching cost ceilings, but shallow: `OperationFacts::schema` loses its static shape, `fs::valid`, `catalog.rs`, `tool_specs.rs`, `provider.rs:668`, the reducer's `ToolStarted` validation, the Tool corpus and three wire corpora all take a new parameter, and the catalog digest becomes route-dependent. It also optimizes around a self-imposed 4,096 cap that no peer applies, leaving the documented 64 KiB limit unreachable on ChatGPT and the MultiEdit 64 KiB live checks ([NEXT_STEPS](../../NEXT_STEPS.md)) impossible there.

### Comparison and choice

| Axis | A | B |
|---|---|---|
| Depth | One constant and one invariant behind the existing `CallLimits` interface; three duplicate invariants deleted | New parameter threaded through five modules and three corpora; Tool interface widens |
| Seam | Stays at `CallLimits` (Provider identity), where the cap already lives and is journaled | Adds a route-to-Tool coupling the catalog, reducer and replay must all share |
| Replay and journal | No new field; replay range widens to `1..=32_768`, existing records valid | Derivable from the pinned cap, but every `ToolStarted` validation changes meaning per Run |
| Cost and usage | Per-call ceiling rises (bounded by Goal budget, deadline, 1 MiB); matches peers | Unchanged ceiling; ChatGPT writes capped near 12 KiB forever |
| Peer parity | Copies Claude Code's 32,000 default and Codex's uncapped behavior, finitely | Diverges from both peers |

Chosen: Design A. Design B's only advantage (unchanged per-call cost) is not a guarantee Arany holds on the ChatGPT route (D-18) and both peers already accept the larger budget.

## Chosen direction

- Simplest sufficient solution after Ponytail: raise `MAX_OUTPUT_TOKEN_CAP` to 32,768, declare it on the three in-scope identities, request it for editor children, delete the duplicated `LocalAcceptanceOnly <= 4096` checks, pin the derivation in one test, and update the three spec sentences. No new type, trait, setting or field.
- Linux/macOS impact: none; the change is pure request sizing and validation.
- Ownership: `provider.rs` owns the ceiling and the derivation invariant; `CallLimits` stays the only interface callers read. The model learns the effective limit from the unchanged Tool schema text ("at most 65536 bytes"), which becomes true on every in-scope route; no catalog or prompt change.
- Alternatives for hard-to-reverse decisions: none are hard to reverse; lowering the constant later needs only the same single edit and a spec line.

## Security and resource model

Trust boundaries, credentials, permissions, Guard frames and `ToolStarted` payload ceilings are unchanged (#234 sized them for 64 KiB). Bounds that move: visible output per primary and editor call (32,768 tokens on all three routes), the opaque turn bound (`max_turn_bytes` = 320 KiB, still under the 1 MiB ceiling) and the derived continuation reserve. Overflow behavior is unchanged and honest (`local_output_limit`, response-contract failure, `InvalidOutcome`). No security pass trigger from [security.md](../../agents/security.md#security-pass-triggers) applies.

## Risks and mitigations

- Higher per-call plan consumption on ChatGPT for a runaway answer → bounded by the Goal token budget, 600-second ceiling and 1 MiB response bound; document the new value in chatgpt.md; identical to peer defaults.
- Larger continuation reserve triggers in-Run Tool compaction earlier on routes without `context_window_tokens` → existing `reserve > limit → limit / 2` path (`tool_compaction.rs:189`); add a `turn_bound_follows_max_output_tokens` row at 32,768 and keep `engine_run.rs` tool-compaction scenarios green.
- A native model whose output ceiling is below 32,768 rejects the request → HTTP 400 is an honest `Rejected` failure surfaced on the Run; current admitted models accept 128K (Anthropic) and 16,384-plus (OpenAI).
- Old journals pin 4,096 or 16,384 → remain inside the widened range, no migration; an older binary reading a new 32,768 record takes the existing incompatible-history archive path ([execution.md:185](../../docs/specs/execution.md)).
- ChatGPT account-admission contract version ([chatgpt.md:53](../../docs/specs/chatgpt.md)) → the counting semantics (visible output = total minus reasoning) are unchanged and the cap value is not an input to the `account_consent` fingerprint as the spec lists it; the implementer confirms this at the fingerprint owner before merging and bumps only if the value turns out to be bound.

## Vertical slices

1. **Primary calls cover the 64 KiB Write on all three routes.** Path: Provider identity (`openai.rs`, `anthropic.rs`, `subscription.rs`) → Engine admission and `Routing::bind` → `RunStarted.output_token_cap` → replay validation → Tool execution. Code: `MAX_OUTPUT_TOKEN_CAP = 32_768` with the derivation test in `provider.rs`; ChatGPT identity declares it and `invoke_at_observed` rejects above the identity cap; delete the duplicate invariants in `routing.rs:190` and `engine.rs:2533`. Spec: execution.md:151 (route caps, replay range `1..=32,768`), chatgpt.md:59 (primary 32,768 visible; review 256; compaction unchanged), tools.md:916 (one clause: the route caps cover this limit, link to execution.md). Tests: identity assertions in `openai.rs:346` and `anthropic.rs:391`; subscription visible-cap row at 32,768 and a request above it rejected; `engine_run.rs` subscription replay scenario at the new cap; a reducer row that `RunStarted` with `output_token_cap: 32_769` is rejected; wire rows rendering and decoding a plain 64 KiB `arany_write` at the real cap on both wire formats; `turn_bound_follows_max_output_tokens` row. One PR.
2. **Private editors get the write cap; researchers keep 4,096.** Path: `children.rs:2398` request cap by `child.editor` → `bind` narrowing → `ProviderCallRecorded` → editor Write through the worker handover. Spec: execution.md:151 and chatgpt.md:59 child sentence ("researchers 4,096; editors the route cap"). Tests: extend `provider_declared_output_cap_reaches_the_primary_and_caps_children` to assert editor and researcher caps; an editor-delegation scenario with a 64 KiB Write in the worker corpus. One PR, stacked on slice 1; small enough to fold into slice 1 if the reviewer prefers.

## Owner live check after the fix

Run on each in-scope route (ChatGPT-plan, native OpenAI, native Anthropic) from the repository root; the file is realistic source, not random bytes:

```sh
mkdir -p .lavish && head -c 65536 crates/arany/src/provider.rs > .lavish/src.txt && wc -c .lavish/src.txt
```

Prompt: "Read `.lavish/src.txt` in full, paging with offset and limit until you reach the end, then Write an exact byte-for-byte copy to `.lavish/big.txt` in a single Write call. Do not abbreviate, summarize or reformat anything; the two files must be identical."

Verify: `cmp .lavish/src.txt .lavish/big.txt && echo identical`; `arany show` lists one `ToolFinished` for the Write with 65,536 bytes and a `ProviderCallRecorded` whose visible output is below 32,768 and whose status is not `local_output_limit`. Failure path: repeat with `head -c 65537`; the Write must be refused with the 65,536-byte limit and the model must report the refusal rather than truncate. Then close [NEXT_STEPS item 8](../../NEXT_STEPS.md#daily-use-first) and record the route evidence there.

## Exit criteria

- CI green on both slices with the tests above; `cargo fmt`, Clippy and the targeted suites locally ([verification](../../docs/development.md#verification)).
- Spec sentences updated in execution.md, chatgpt.md and tools.md; no new stored field, payload version or migration (documented in the execution.md version paragraph as a range widening).
- Live copy check passes on the three routes and the failure path refuses honestly; results recorded in NEXT_STEPS.md by the owner.
- Plan deleted under the [planning lifecycle](../README.md#lifecycle) once the live check lands.

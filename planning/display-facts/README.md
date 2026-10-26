# Display facts

## Metadata

- Status: `Ready` (implementation gated as listed under Gates)
- Owner: harness maintainers
- Last updated: 2026-10-26

## Outcome and scope

One observable result: adding a Tool operation with an existing wire shape touches no presentation consumer outside its family file; policy consumers still gain their exhaustive arms. The library exports read-only `DisplayFacts` per `ToolCall` (family, site, verb, subject text, paths), consumed by Tool rows, serve items, ACP projection and journaled report labels. Approval disclosures (host warnings, inert output, per-family escaping) stay in the binary per family. The harness responsibility served is the Tool surface ([priority 1](../../AGENTS.md#product-and-implementation-priorities)) ahead of [shared-workspace-editors](../shared-workspace-editors/README.md) S2 removing five variants and [network-proxy](../network-proxy/README.md) S2 adding `WebFetch`/`WebSearch`; remote MCP `url` support belongs to that plan's S3. Owner decision, 2026-10-26.

Excluded: authority, policy and admission consumers (`tools/permissions.rs`, `tools/config.rs`, `session/events/goal.rs` plan-contract scope, `step/intent.rs`); model-facing schemas and `tools/snapshots/*.json`; changes to journal bytes or the `serve` schema fixture. A new wire shape requires its own contract change.

## Research

- At `dfba8416`, literal `ToolCall::` occurrences in the named production functions are: [tool_rows.rs](../../crates/arany-cli/src/presentation/tool_rows.rs) `tool_summary` 21, `read_operation` 10, `group_label` 10; [summary.rs](../../crates/arany-cli/src/presentation/tool_rows/summary.rs) 32 across its family functions; [serve/items.rs](../../crates/arany-cli/src/cli/serve/items.rs) `tool_item_with_denial_output` 11; [acp/tools.rs](../../crates/arany-cli/src/cli/acp/tools.rs) `project` 31; [engine/report.rs](../../crates/arany/src/engine/report.rs) `goal_report_call_label` 30. Count the literal in each function body, including each alternative in an arm. These are syntax counts, not counts of independent consumers.
- [tools/catalog.rs](../../crates/arany/src/tools/catalog.rs) owns `Family`, `Site` and operation facts. [tools/types.rs](../../crates/arany/src/tools/types.rs) owns `ToolCall`, its admitted argument bounds and changeset path-count bounds. [Overview §8](../../docs/architecture/system-overview.md#8-terminal-presentation) currently says operation facts do not cross into presentation; D1 changes that sentence for display data only.
- [Program::allows](../../crates/arany/src/tools/config.rs) is exhaustive. Policy consumers retain compile-time coverage. Serve's typed command/file-change/MCP branches carry fields beyond display facts, and `goal_report_evidence` inspects typed results; neither can be replaced by a label-and-path value.
- Peer precedent: [Codex history cells](https://github.com/openai/codex/blob/main/codex-rs/tui/src/history_cell/mod.rs) separate exec, MCP, patches and search rendering behind the history-cell interface. This plan follows per-family rendering locality; Arany's separate journaled report also needs shared inert facts, without moving CLI formatting into the Engine.

## Contract and scenarios

- Owning contracts: [terminal.md](../../docs/specs/terminal.md) Tool rows (unchanged bytes), [serve.md](../../docs/specs/serve.md) item kinds (unchanged schema), [acp.md](../../docs/specs/acp.md) Tool projection (unchanged), overview §8 (display facts cross as data; authority does not).
- Successful path: every existing Tool summary, approval line, serve item, ACP item and Goal report renders byte-identically. The synthetic-variant check below covers reuse of an existing family and wire shape.
- Rejection: missing subject or paths for a path-bearing call fails the facts-table completeness test; controls and oversized subjects still take the consumer's existing escape/truncation path. New policy variants fail compilation until admitted explicitly.
- Evidence owners: [Tool row tests](../../crates/arany-cli/src/presentation/tool_rows/tests.rs), [serve history](../../crates/arany-cli/tests/serve/history.rs), [ACP projections](../../crates/arany-cli/tests/acp/in_process.rs), [Goal report tests](../../crates/arany/src/engine/report.rs), and the existing [Tool facts-table corpus](../../crates/arany/src/tools/tests.rs), extended beside the display-facts owner.

## Chosen direction

- Simplest sufficient solution: `pub struct DisplayFacts<'a> { family: Family, site: Site, verb: &'static str, subject: Cow<'a, str>, paths: Vec<&'a str> }` returned by synchronous `ToolCall::display_facts()` beside `facts()`. Borrow strings from the admitted call; a small vector accommodates a single path, Move's two distinct fields and changeset paths without claiming they already form a borrowed slice. Build composite subjects only from bounded admitted arguments. No trait or registry.
- Ownership and interface: Tools owns extraction and completeness. The library export crosses the in-process boundary to CLI rows and the serve/ACP adapters; the Engine report consumes the same data locally. No transport/database types cross it. Consumers own formatting, ordering, escaping and truncation; facts own no mutable state. The synchronous projection has no I/O, cancellation, retry or new error channel; call admission remains the existing prerequisite.
- Bounds and overflow: derive subject construction and path-vector bounds from `ToolCall::argument_limit()`, `MAX_CHANGESET_FILES` and the corresponding admitted call fields in `tools/types.rs`; do not copy payload content into subjects. Existing consumer limits truncate or reject at their current boundaries. Work and allocation are linear in those bounded fields; no cache or queue is added.
- No OS behavior change; existing Linux/macOS output corpora verify the adapters. No native support claim is added.

## Security and resource model

Display facts add no authority, grant, digest, receipt or credential access; policy and admission still use typed calls. The only crossing is untrusted display data derived from already admitted arguments, with no new effect, credential access, persistence, network, descendant or aggregate-storage surface. `subject` and `paths` are raw untrusted text; every consumer keeps its existing escaping and bounds, and the [untrusted-rendering security pass](../../agents/security.md#security-pass-triggers) runs in D1 (Tool rows, serve items) and D2 (ACP, report).

## Risks and mitigations

- Generic rendering flattens a family-specific disclosure → approvals stay per family in the binary. Typed wire payloads and result evidence stay with their existing adapters.
- Byte drift in a projection → use existing exact-output owners, including hostile controls, path lists, denial and Uncertain results; no golden is regenerated.

## Consumer inventory

This is the removal checklist; file-wide `ToolCall::` absence is not the criterion. Each slice records the final matching functions and their remaining role against this inventory.

| Owner | Replace with facts | Retain or route through its family owner |
|---|---|---|
| `tool_rows/summary.rs::operation` | Shared family/site/verb/subject/path extraction | Family formatting for Search options, MultiEdit counts, Command argv and Git branch/staged/host labels |
| `tool_rows.rs::{tool_summary,read_operation,group_label}` | Common operation label/path extraction via `summary::operation`; route read/group and result classification through the summary family owner | Observation status, output parsing, limits, grouping order and approval/host disclosures; any variant-specific classifier needed by a new operation belongs in its family owner |
| `cli/serve/items.rs::tool_item_with_denial_output` | Repeated path extraction for file-change items | Typed command argv/cwd/attestation, change kind and MCP arguments; generic `Item::ToolCall` retains the serialized call. Family-specific extraction lives behind family dispatch |
| `cli/acp/tools.rs::project` | Title/detail/path extraction from a typed call | ACP kind mapping behind family dispatch; raw-input serialization, fencing, notification sizing and status remain in ACP |
| `engine/report.rs::goal_report_call_label` | Entire variant label/subject match | Report-specific wording through family formatting; `goal_report_evidence` and other typed policy/result matches in this file are outside label extraction |

## Vertical slices

1. **D1: library facts and the two binary row owners.** Add `ToolCall::display_facts()` and extend the existing variant corpus for completeness. Apply the D1 rows of the consumer inventory to `tool_rows/summary.rs`, `tool_rows.rs` and `cli/serve/items.rs`, preserving typed wire branches. Run the untrusted-rendering security pass. Same-PR docs: overview §8's boundary sentence; edit terminal/serve evidence links only if their owner moves. ≤600 changed lines.
2. **D2: ACP projection and journaled report labels.** Apply the ACP and report rows of the inventory, keeping kind-specific mapping behind family dispatch and typed result evidence intact. Run the untrusted-rendering security pass. Same-PR docs: adjust ACP evidence links only if their owner moves. ≤400 changed lines.

## Exit criteria

- Inventory remaining `ToolCall::` matches in the named functions with `rg -n 'ToolCall::'` over their files and compare each with the consumer table; there are no unlisted display-extraction matches in shared consumers. A synthetic variant in a scratch branch needs no edit in the presentation consumers named above outside its family file; policy consumers (`tools/permissions.rs`, `tools/config.rs`, `session/events/goal.rs`, `step/intent.rs`) keep exhaustive matches and must fail to compile until edited. The scratch variant reuses an existing family and wire shape; catalog/model schema registration still occurs in its own owners.
- Existing exact-output corpora pass unchanged; `crates/arany-cli/tests/fixtures/serve/schema.json` and `tools/snapshots/*.json` are byte-identical.
- Close under the [planning lifecycle](../README.md#lifecycle) when D2 and the exit criteria are complete.

## Gates

Opens after [shared-workspace-editors](../shared-workspace-editors/README.md) S2 removes the worker variants and must land before [network-proxy](../network-proxy/README.md) S2 adds `WebFetch`/`WebSearch`. Coordinate `tool_rows/*` with architecture-residue R4 only if that slice touches presentation. Test-corpus-pruning T5c/T5d follow D2/D1 respectively so projection evidence is moved after its consumer changes.

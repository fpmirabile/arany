# Spec limits generation and size budget

## Metadata

- Status: `Ready`
- Owner: harness maintainers
- Last updated: 2026-10-25

## Outcome and scope

One observable capability: every public numeric bound in `docs/specs/` is rendered from the Rust constant that owns it, and CI fails when a spec restates a bound by hand or grows past its word budget. The harness responsibility served is maintainability of the contracts the Engine, Tools and Session are reviewed against ([global rule 8](../../AGENTS.md#global-rules): name every limit once beside its owner and derive every other use from that constant, including user- and model-facing text). The specs are the last remaining hand-written copy.

Included: a `limits` export in the binary, a committed generated file, a CI spec-budget check and the prose edits that replace restated numbers with links. Excluded: changing any bound's value, moving behavior between specs, or deleting scenario rows; the budget forbids growth, it does not demand cuts beyond the restated numbers.

## Research

- Size on `main` at `f0b7a348`: `docs/specs/*.md` total 136,847 words (`wc -w`); `tools.md` 43,202, `terminal.md` 21,092, `execution.md` 16,981, `provider.md` 9,272, `goals.md` 8,129, `session.md` 7,028. The [architecture overview](../../docs/architecture/system-overview.md) adds 8,659.
- Hand-restated bounds: a regex over `N (KiB|MiB|GiB|bytes|seconds|ms|entries|files|tokens|lines|rows|chars)` finds 218 numeric bound mentions in `tools.md`, 82 in `execution.md`, 51 in `terminal.md`, 23 in `serve.md`, 21 each in `provider.md` and `session.md`, 20 in `goals.md`. Only seven specs have a dedicated limits section (`tools.md` [Limits and failure behavior](../../docs/specs/tools.md#limits-and-failure-behavior), `execution.md` [Persistence bounds](../../docs/specs/execution.md#persistence-bounds), `terminal.md` Public limits and compatibility, `serve.md`, `acp.md`, `credentials.md`, `telemetry.md` Export bounds); the rest state bounds inline.
- The same number appears in several homes today: the 64 KiB Tool result ceiling is `MAX_TOOL_RESULT_BYTES` ([tools/types.rs:18](../../crates/arany/src/tools/types.rs)), restated in `tools.md` limits, `session.md` [Context and compaction](../../docs/specs/session.md#context-and-compaction) and `execution.md` persistence bounds (as "twice the Tool result ceiling"). The 10,000 Events-per-lineage bound is `MAX_SESSION_EVENTS` ([store/replay.rs:11](../../crates/arany/src/store/replay.rs)) and an `execution.md` row. The code-health survey already found the Guard enforcement digest hand-encoding limits as prose bytes (`.lavish/code-health-2026-10-23.md`, finding 14).
- Existing generation pattern: `arany generate-schema` writes the serve protocol JSON schema and [`tests/generate_schema.rs`](../../crates/arany-cli/tests/generate_schema.rs) asserts the output equals the committed `fixtures/serve/schema.json`. The same binary-export-plus-fixture pattern fits a limits table.
- CI: `ci.yml` runs `typos` 1.35.8 in a bounded step and `scripts/ci-changes.py` decides whether code changed; there is no link, anchor or size check for documentation ([development.md verification](../../docs/development.md#verification) asks reviewers to check links by hand).
- Peers: Codex generates its configuration reference from types (`codex-rs/config-schema` crate, [config reference](https://learn.chatgpt.com/docs/config-file/config-reference)) and keeps in-repo docs as stubs; Claude Code documents each bound once on its [tools reference](https://code.claude.com/docs/en/tools-reference) and [settings](https://code.claude.com/docs/en/settings) pages with the environment variable that owns it. No peer carries a 40,000-word contract file.

## Contract and scenarios

- Owning contract: the [spec workflow](../../docs/specs/README.md) ("Reference code/configuration for incidental constants") and global rule 8. Proposed change: the workflow states that a spec names a bound by linking its row in the generated limits file, never by restating the number; `docs/specs/limits.md` is generated and read-only.
- Successful path: `arany generate-limits` prints the Markdown table; the committed file matches; every spec word count is at or below its recorded budget; CI is green.
- Rejection: a PR that edits a constant without regenerating fails `generate_limits_matches_committed`; a PR that adds 200 words to `tools.md` fails `spec-budget` with the file name, the budget and the excess; a spec that restates a listed bound as a bare number fails the restatement scan with the line.
- Evidence owners: `crates/arany-cli/tests/generate_schema.rs` (extended for limits), `scripts/test-ci-changes.py` pattern for the budget script's own tests, the CI documentation lane.

## Chosen direction

- Simplest sufficient solution: a `LimitsTable` in the library listing `(domain, name, value, unit, owner path)` built from the existing `pub const` items by reference, not by parsing source; a `generate-limits` subcommand beside `generate-schema`; a Python script in `scripts/` for word budgets and restatement scanning, run in the existing documentation CI lane. No new dependency, no doc framework, no build script.
- OS impact: none; documentation and a pure-Rust export.
- Ownership: the library owns the table (it owns the constants); the binary owns the subcommand; `scripts/` owns the budget check. Not a module boundary change.

## Security and resource model

No trust boundary changes. The export reads compiled constants only. The budget script reads repository Markdown in CI; bounded by the repository itself.

## Risks and mitigations

- Specs reference a generated file while in-flight PRs still restate numbers → the restatement scan starts in warn-only mode in S1 and turns blocking in S3 after the follow-up families below have merged their same-PR spec edits.
- A bound has several owners in code (the 4096-byte path ceiling appears at 24 inline sites per the code-health survey) → the table lists one named constant per row; a row whose owner does not exist yet is added only after that constant is extracted (S2 does this for the limits it moves).
- Budgets freeze needed clarifications → budgets are per file and a PR may lower one file's budget to raise another's by the same amount in the same PR; the total never rises.

## Vertical slices

1. **Limits export for the Tool domain.** `LimitsTable` with the Tool-family rows (`MAX_TOOL_RESULT_BYTES`, `MAX_TOOL_ARGUMENT_BYTES`, `MAX_READ_PAGE_LINES`, `MAX_READ_LINE_CHARS`, catalog, snapshot, process-profile and MCP bounds that already have constants); `arany generate-limits` subcommand; committed `docs/specs/limits.md`; `generate_limits_matches_committed` beside the schema test; `tools.md` [Limits and failure behavior](../../docs/specs/tools.md#limits-and-failure-behavior) rows for those bounds become links to the generated rows. Files: `crates/arany/src/limits.rs` (new, small), `crates/arany-cli/src/cli/generate_schema.rs` (sibling subcommand), `crates/arany-cli/tests/generate_schema.rs`, `docs/specs/limits.md`, `docs/specs/tools.md`, `docs/specs/README.md`. ≤600 changed lines.
2. **Session, execution, Goal and Provider rows.** Add `MAX_CONTEXT_CONTENT_BYTES`, `MAX_USER_MESSAGE_BYTES`, `MAX_SESSION_EVENTS`, `MAX_STEP_TOOL_CALLS`, compaction constants, `MAX_PROVIDER_REQUEST_BYTES`, Goal budget defaults and the persistence-bounds rows; replace the restated numbers in `execution.md` [Persistence bounds](../../docs/specs/execution.md#persistence-bounds), `session.md`, `goals.md` and `provider.md` with links. Where a bound has no single constant, extract it beside its owner first (one `pub(crate) const`, no behavior change). ≤600.
3. **Spec budget check in CI.** `scripts/spec-budget.py` reads `docs/specs/budget.toml` (one word budget per spec, initialized to the current count rounded up to the next 500) and fails on growth or on a bare number next to a unit that matches a generated row; `scripts/test-spec-budget.py` pins the rules; the documentation lane in `ci.yml` runs both. `docs/development.md` verification section names the check; `agents/testing.md` documentation checks link it. ≤400.
4. **Split `tools.md` under budget.** Move the native sections ([macOS enforcement](../../docs/specs/tools.md#macos-enforcement-and-compatibility), [Linux enforcement](../../docs/specs/tools.md#linux-enforcement-and-compatibility), [Shared native capacity](../../docs/specs/tools.md#shared-native-capacity-and-settings), [Local storage inventory](../../docs/specs/tools.md#local-storage-inventory), [Persistent Workspace build cache](../../docs/specs/tools.md#persistent-workspace-build-cache)) to `docs/specs/tools-native.md` as a pure move with inbound links, `AGENTS.md` routing row and documentation map updated; `budget.toml` splits the `tools.md` budget accordingly. Prose moves count once toward the ≤600 ceiling only for link rewrites; the moved text itself is excluded by the budget script's move detection (identical paragraphs).

## Exit criteria

- `arany generate-limits` output equals `docs/specs/limits.md`; CI fails when they differ (owner: `generate_schema.rs` test).
- `scripts/spec-budget.py` passes on `main` and fails on a synthetic 1-word overrun and on a synthetic restated bound (owner: `scripts/test-spec-budget.py`).
- No spec restates a bound that has a generated row; `tools.md` is under its split budget.
- Contract updates: `docs/specs/README.md` names the generated file and the budget rule; `docs/development.md` names the check. Close the plan by deleting this folder when slice 4 lands.

## Gates

No code owner conflict. Prose conflicts: slices 1 and 2 edit the limits sections that the architecture follow-ups [P5, T2, C3 and J1–K2](../architecture-homogenization/README.md#follow-up-slices-post-refactor-review) also edit in their same-PR documentation; land slice 1 now and rebase slice 2 after K1/K2 to avoid conflicting `execution.md` rows. Slice 3's blocking mode waits for the follow-up families that are still open at that time to merge their spec edits. Slice 4 waits for C3 (the last follow-up that edits the native `tools.md` sections).

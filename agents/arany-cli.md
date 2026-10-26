# Binary practice

Load for work in `crates/arany-cli/`: `cli/`, `terminal/`, `presentation/` and `accounts/`. Read the [terminal](../docs/specs/terminal.md), [credentials](../docs/specs/credentials.md) or [ChatGPT](../docs/specs/chatgpt.md) contract for the affected flow and [security rules](./security.md) when triggered. The [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns runtime composition and dependency direction.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; load [fixture procedures](./references/testing-details.md) for native or process work.

Present sizes, durations, counts and text limits in human terms through shared presentation formatters; audit every human view against the [measure formats](../docs/specs/terminal.md#human-readable-measures).

## Growth

The [library growth rule](./arany.md#growth) applies unchanged: past the [rule 8](../AGENTS.md#global-rules) signals, add the owner for the new flow instead of extending the file. The binary's owners are:

- Terminal: a feature adds `terminal/<feature>.rs` (canon: `terminal/graphics.rs`) and keeps its state inside the existing lifecycle or panel owner in `terminal.rs`.
- Attached setup and provider UI: new setup flows get `setup/<flow>.rs` (canon: `cli/attached/setup/native.rs`); [architecture-residue R2](../planning/architecture-residue/README.md#r2--finish-u1u2-with-stable-setup-entry-points) names the remaining owners.
- Run driving: a phase gets `cli/attached/run/<phase>.rs` (canon: `cli/attached/run/input.rs`); new Run-side state joins its phase owner.
- Presentation: a new operation adds an arm in its family file (canon: `presentation/tool_rows/summary.rs`); dispatch over the library's closed `Family` stays exhaustive, and a new family gets a child file.
- Serve and ACP: a projection change edits its item owner (canon: `cli/serve/items.rs`) and the ACP translator's exhaustive typed match; apply the [Tool match rule](./arany.md#growth).

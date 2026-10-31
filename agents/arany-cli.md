# Binary practice

Practice for `crates/arany-cli/` (`cli/`, `terminal/`, `presentation/`, `accounts/`); read only the section your edit needs. Open the [terminal](../docs/specs/terminal/README.md), [credentials](../docs/specs/credentials.md) or [ChatGPT](../docs/specs/chatgpt.md) section for the affected flow and [security rules](./security.md) only when triggered. The [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns runtime composition and dependency direction.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; load [fixture procedures](./references/testing-details.md) for native or process work.

Load the `terminal-ui-design` skill for terminal layout and interaction work. Read the owning [terminal spec](../docs/specs/terminal/README.md) section before every design or review, apply [AGENTS.md priorities](../AGENTS.md#product-and-implementation-priorities) (Ponytail, OS parity) and [testing](./testing.md) before verification claims, and keep screenshots in gitignored `.lavish/`. The [library skill notes](./arany.md#skills) apply to Rust here too.

Present sizes, durations, counts and text limits in human terms through shared presentation formatters; audit every human view against the [measure formats](../docs/specs/terminal/entry-and-modes.md#human-readable-measures).

## Growth

The [library growth rule](./arany.md#growth) applies unchanged: past the [rule 8](../AGENTS.md#global-rules) signals, add the owner for the new flow instead of extending the file. The binary's owners are:

- Terminal: a feature adds `terminal/<feature>.rs` (canon: `terminal/graphics.rs`) and keeps its state inside the existing lifecycle or panel owner in `terminal.rs`.
- Attached setup and provider UI: new setup flows get `setup/<flow>.rs` (canon: `cli/attached/setup/native.rs`).
- Run driving: a phase gets `cli/attached/run/<phase>.rs` (canon: `cli/attached/run/input.rs`); new Run-side state joins its phase owner.
- Presentation: a new operation adds an arm in its family file (canon: `presentation/tool_rows/summary.rs`); dispatch over the library's closed `Family` stays exhaustive, and a new family gets a child file.
- Serve and ACP: a projection change edits its item owner (canon: `cli/serve/items.rs`) and the ACP translator's exhaustive typed match; apply the [Tool match rule](./arany.md#growth).

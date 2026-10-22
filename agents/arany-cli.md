# Binary practice

Load for work in `crates/arany-cli/`: `cli/`, `terminal/`, `presentation/` and `accounts/`. Read the [terminal](../docs/specs/terminal.md), [credentials](../docs/specs/credentials.md) or [ChatGPT](../docs/specs/chatgpt.md) contract for the affected flow and [security rules](./security.md) when triggered. The [system overview](../docs/architecture/system-overview.md#2-runtime-architecture) owns runtime composition and dependency direction.

[Testing](./testing.md#corpus-placement) owns corpus placement and shared support; load [fixture procedures](./references/testing-details.md) for native or process work.

Present sizes, durations, counts and text limits in human terms through shared presentation formatters; audit every human view against the [measure formats](../docs/specs/terminal.md#human-readable-measures).

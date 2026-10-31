# Arany next steps

This file lists unresolved defects, deferred functionality and code work for
the development beta and broader release. [Domain specs](./docs/specs/README.md)
own behavior. Remove an item when its completion condition is met; completed
work belongs in Git history.

## Next functional priority

The [non-negotiable guarantees](./AGENTS.md#non-negotiable-guarantees) bound every item. [Focused plans](./planning/README.md#active-plans) track in-flight work.

### Daily use first

Done when a normal engineering day in this repository (explore, multi-file edits, tests, commit and PR) needs no fallback to another harness; then the owner's other repositories. Native evidence means commands ran in the sandbox, not the host fallback.

1. **Linux Workspace overlays.** Done when the unreachable Linux primary selected-copy path (`SELECTION_READY`, copy baseline and capture) is gone, and stock Ubuntu 24.04 with the executable profiles and an unrestricted distribution both pass the [Ubuntu procedure](./docs/specs/tools/native-guard.md#ubuntu-namespace-entry-diagnosis) and the [offline Cargo journey](./crates/arany-cli/tests/session_run/tools/native_exec.rs).
2. **`cargo test` here inside the Guard.** Done when an attached session runs `cargo test --workspace` with `resource_class: "build"` to an executed test count on a native Mac and on Linux. Socket-dependent tests stay ignored until a local test IPC capability exists ([commands](./docs/specs/tools/commands-and-cache.md#commands)).
3. **PATH directories and command tools.** Done when granted `PATH` directories and manager roots work on restricted Ubuntu, the [user-program journey](./crates/arany-cli/tests/session_run/tools_macos/user_programs.rs) passes on the owner's Mac, and a Prefix wrapper runs guarded and on the host ([command tools](./docs/specs/tools/hooks.md#hook-execution)).
4. **Named-host latency.** Done when the owner runs the [corpus procedure](./crates/arany-cli/tests/guard_runs/latency.md#owner-procedure) on the named host and keeps its reports.

### Pursue, in order

1. **Custom-route Tools: live evidence and output cap.** Record one live Read, Edit and Command round trip on DeepSeek, Kimi and GLM before claiming support. Each profile's fixed `max_output_tokens` blocks large Writes; raising it needs a new `provider-profiles.json` version ([Provider contract](./docs/specs/provider/routing-and-usage.md#custom-destinations-and-routing)).
2. **Supervisor.** Signals stay shadow-only until calibrated thresholds are accepted ([Supervisor adapter](./docs/specs/tools/hooks.md#supervisor-adapter)).
3. **Provider roster: deferred work.** Judge-only adapters need a typed-judge contract; a durable per-agent pause, retry and switch after exhausted routes needs accepted Events and commands ([roster presets](./docs/specs/provider/selection.md#named-roster-presets)).
4. **`arany-journal` crate extraction (deferred).** Only under measured dependency, ownership or release pressure, after resolving `tools::Cancellation` ownership ([serve contract](./docs/specs/serve.md)).

### Tool and Engine follow-ups

- **Compaction hook refusal mapping.** [Compaction](./crates/arany/src/engine/compaction.rs) maps `HookRefusal` through `into_engine_error`, while the Run loop keeps typed stop causes. Reconcile without changing budget, cancellation or replay ([Engine hook replay](./crates/arany/tests/engine_run/hooks.rs)).
- **Recorded native Guard storage.** Done when the [storage report](./docs/specs/terminal/commands.md#storage-report) counts Linux seeds and macOS images from their ownership records as live, retained or unknown.
- **Conditional loop refinements.** Revisit RAII `ActiveSegment`, a typed child queue or a repeated-call stop only for a failing dogfooding case.
- **Exec budget-refusal predicate.** `exec` uses the broader `budget_stop_goal`; attached and serve need a committed `budget_refusal`. Unify only if their reload contracts converge.
- **Deferred network grants.** Interactive and background jobs need their own design ([network contract](./docs/specs/tools/commands-and-cache.md#network-grants-and-egress-proxy)).
- **Linux read-folder hiding depth.** Linux hides protected names only at an approved read folder's first level; deeper hiding is deferred ([protected names](./docs/specs/tools/host-commands.md#protected-names-beneath-read-folders)).
- **Debug stack canary.** `heavy_guard_journeys_fit_a_one_and_a_half_mebibyte_stack` ([test](./crates/arany-cli/tests/guard_runs/permissions/hooks.rs)) overflows in debug builds on macOS and Linux; CI runs it only in release. Done when it passes in debug, or the test documents that it is release-only.

### Terminal follow-ups

- **Plan usage shows unknown.** On a ChatGPT plan, `/usage` shows unknown 5-hour and weekly use. Done when the ChatGPT slices of the [reported Provider limits plan](./planning/usage-limits/README.md) land and one live Codex backend Run shows plan windows in `/usage` and `/status`, keeping no raw headers or tokens.
- **API and custom-route rate limits.** OpenAI streaming, Anthropic and custom routes still lack reported rate facts under the same plan ([reported usage](./docs/specs/terminal/conversation.md#reported-usage)).
- **Other terminal hosts (deferred).** Zellij and VS Code's xterm.js need insertion without partial scroll regions; `tmux -CC` needs forced Classic. Support needs a contract and native evidence ([compatibility](./docs/specs/terminal/entry-and-modes.md#public-limits-and-compatibility)).

### Frozen areas

CI stays green; no new features, spec sections or evidence gates for: terminal views and tmux evidence beyond dogfooding; macOS Guard features beyond parity probes; build cache; ChatGPT OAuth and subscription routes; A6 registry fetch; the Anthropic consumer-subscription gate; new typed Tools other than the in-process read set. Typed delete, typed move and parallel reads keep their scope and gates. The freeze stays until the owner lifts it.

Exception: OWNER DECISIONS D2 (2026-10-27) and D4 (2026-10-28) authorize the ChatGPT slices of the [reported Provider limits plan](./planning/usage-limits/README.md): per-account storage identity, the public Codex OAuth client profile, backend catalog, Codex backend inference and plan-window reporting. Its API report slices touch no frozen area.

- **Deferred Anthropic consumer subscriptions.** Native Claude-plan access is user-deferred; API-key billing is a separate route, and another product's credentials never authorize Arany access ([Provider support](./docs/specs/provider/selection.md#provider-support-summary)).

## Beta 2 and broader release

| Pending gate | Completion condition |
| --- | --- |
| Image hover emulator rendering | Owner dogfooding shows correct previews on kitty, Ghostty, Konsole, WezTerm, iTerm2 and foot, and none in Classic, linear, disabled-mouse or tmux sessions. [Contract](./docs/specs/terminal/drafts-and-paste.md#image-hover-preview) |
| Hosted CI | Three sequential complete `ci.yml` runs pass on the final head, `linux_native_advanced=true` included. The [macOS proxy corpus](./crates/arany/src/tools/network/proxy/tests_macos.rs) covers redirect admission, connection, byte and idle bounds, and proxy death. The ignored Linux test `retained_failed_unit_is_reset_before_capacity_is_reclaimed` passes. [Workflow](./.github/workflows/ci.yml) |
| Native stores and clipboard | Repeated same-head `macOS Tests (debug)` runs, a count fixed beforehand, pass the selected-item journey. Manual: human Keychain dialogs and desktop clipboard interoperability. [Credentials](./docs/specs/credentials.md#selected-item-authorization), [clipboard](./docs/specs/terminal/drafts-and-paste.md#paste-and-attachments) |
| Distribution and dependency policy | [`deny.toml`](./deny.toml) drops the RUSTSEC-2026-0370/0371/0372 ignores once `Cargo.lock` holds no `zbus` 5.x; the [release-policy workflow](./.github/workflows/release-policy.yml) and [bundle script](./scripts/release-bundle.sh) pass for the release commit. |
| Terminal Markdown and accessibility | On native Linux and macOS emulators, streamed Markdown and the host approval dialog render correctly at 16 to 120 columns in Fullscreen, its pager and Classic, through resizes, under `NO_COLOR` and with a real screen reader. TestBackend frames do not count. [Terminal contract](./docs/specs/terminal/conversation.md#terminal-continuity-and-provisional-text) |
| Other platforms | `Linux ARM64 Tests (debug)` and `(release)` (`linux_arm64=true`) pass on native `aarch64` hosts without emulation or skips. [Native CI procedure](./agents/references/testing-details.md#local-verification-and-ci) |

Neither implemented code nor an old passing run establishes broader support.

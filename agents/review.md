# Review practice

Load when reviewing a diff, a branch or a pull request, by hand or through an automated reviewer. [AGENTS.md](../AGENTS.md) owns the guarantees and global rules; this file names where they bite in review. Flag any change that weakens a guarantee, even for convenience or speed. Do not flag style that rustfmt or Clippy (`-D warnings`) already enforce.

Review Rust with the `rust-clean-code` skill and module boundaries with `clean-architecture-review`, as the [library practice](./arany.md#skills) says. Flag speculative traits, seams, frameworks, configuration or dependencies (Ponytail), but never accept cuts to correctness, security, bounds, error handling or tests to save lines.

## Highest-value checks

From defects this repository actually shipped:

- A rule implemented for the primary agent but not for researcher children, review, compaction, `exec` versus attached mode, or resume and replay. Check every actor the owning spec covers, not only the main path.
- Engine and reducer disagreeing: an Event the Engine can append that the reducer rejects, or the reverse, especially on cancellation, deadline, fallback switch, permission deny and fork paths.
- Cancellation or a timeout racing a durable transition: an append that is journaled but not applied in memory (dropped futures, `tokio::select!`, timeout wrappers around journaling code).
- Journal admission bounds (`Action::max_events_for`) smaller than the Events a step can append.
- Untrusted text (paths, Tool output, MCP metadata, errors, model text) reaching instruction, trust or approval decisions ([security](./security.md)).
- Wall-clock-sensitive tests that flake on slow macOS debug runners.

## Session and reducer

`crates/arany/src/session/`: an Event payload change follows the [versioning rule](../docs/specs/execution/persistence.md#incompatible-history). Reducer validation stays exact: flag loosened checks and confirm each new acceptance is limited to the intended actor and state. Fork projection and replay never depend on parent Events after the fork point.

## Engine

`crates/arany/src/engine/`: the Event order a change produces is accepted by the reducer for primary, researcher, review and compaction actors. Budgets, active time and call reservations are charged exactly once; fallback switches are journaled before dispatch and never raced by cancellation; admission reserves enough Events.

## Store

`crates/arany/src/store/`: every load path runs the same strict validation. Private files and directories keep owner-only modes and ACL checks, symlinks are refused, and record and database bounds stay explicit.

## Tools and private files

`crates/arany/src/tools/` and `private_fs`: effects run only in the Guard under a pinned, typed grant, except the host operations the isolation guarantee names. Read-only operations never widen disclosure, and unsupported enforcement fails closed. Linux (bwrap, seccomp) and macOS (Seatbelt, launchd) keep equivalent behavior; flag a platform-only change without a reason.

## Provider

`crates/arany/src/provider/`: credentials never reach arguments, Events, logs, telemetry or error bodies. Fallback happens only on 5xx before body admission, and Tool-enabled chains keep one adapter and account. Requests stay byte-stable for prompt caching.

## Binary

`crates/arany-cli/`: the library crate never depends on ratatui, crossterm or keyring. Terminal text derived from paths, errors or model output goes through `escape_terminal`. A rendering change is checked in linear and screen-reader mode, Classic and Fullscreen, and small geometries; review layout and interaction with the `terminal-ui-design` skill under the [binary practice](./arany-cli.md).

## Specs

`docs/specs/`: the [spec workflow](../docs/specs/README.md) owns content rules. Flag a number or rule duplicated from another owner (a duplicated limit already drifted once), a behavior change in code without its owning spec, and a spec that no longer matches the code.

## Tests

A fix needs a regression test that fails without it. Flag tests that sleep, depend on wall-clock timing or test order, or weaken an assertion to pass; [testing](./testing.md) owns the rest.

## Workflows

`.github/workflows/`: actions and toolchains stay pinned with least-privilege permissions, and the library dependency-boundary check and code-change detector stay consistent with the workspace layout.

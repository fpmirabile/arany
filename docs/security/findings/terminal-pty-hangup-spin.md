# Finding: PTY loss can strand a dependency cursor query

Status: **published source selected; Linux restoration regression fixed; native macOS and acquisition-fault verification remains open**. Severity of the original defect: **high for local availability**.

## Boundary and current selection

Arany selects unmodified published `crossterm 0.29.0` with its registry checksum in [Cargo.lock](../../../Cargo.lock). The maintainer chose upstream maintenance over a checked-in vendor repair. Arany's [Unix input owner](../../../src/terminal/input/raw.rs) owns bounded polling, EOF/permanent-error reporting and joined shutdown directly; canonical input has its own bounded reader. Crossterm remains responsible for terminal operations and the initial cursor query.

The published Unix Mio event backend ignores zero-byte reads and some permanent errors. [Upstream issue #1110](https://github.com/crossterm-rs/crossterm/issues/1110) and [repair PR #1116](https://github.com/crossterm-rs/crossterm/pull/1116) identify the upstream responsibility. Arany carries no path/git replacement or alternate event-backend dependency. These changes do not repair the upstream backend or certify arbitrary acquisition-fault timings.

## Restoration failure and repair

Ratatui 0.30's `Terminal::clear` queries the backend cursor position before clearing, even for a fullscreen viewport. Calling it after stopping Arany's input reader reenters Crossterm's event backend during cleanup. A lost PTY can strand that synchronous query before the active Run future consumes cancellation and closes its local Provider socket.

The [terminal owner](../../../src/terminal.rs) now clears the fullscreen backend directly before showing the cursor and flushing. The terminal is then discarded, so resetting Ratatui's frame buffers and preserving a queried cursor position serve no purpose. Cleanup performs no input query. Initial acquisition still finishes before Arany starts its sole reader.

## Evidence

Tests: [`tests/session_run/active_terminal.rs::inline_sigterm_restores_raw_mode_during_active_run`](../../../tests/session_run/active_terminal.rs); [`tests/session_run/active_terminal.rs::lost_pty_during_provider_call_cancels_without_answer`](../../../tests/session_run/active_terminal.rs). Signal restoration permits only the acquisition cursor query and preserves terminal settings and cancelled replay; PTY loss must close the active local Provider socket without fabricating an answer.

The idle, setup and active Linux loss owners remain enabled in [acquisition](../../../tests/session_run/active_terminal/acquisition.rs) and [active terminal](../../../tests/session_run/active_terminal.rs). They bound product exit, cancel the active synthetic local Provider socket, reject a fabricated answer and independently guard the exact test-owned product PID. These observations prove those Linux paths, not remote generation/billing cessation, arbitrary cursor-acquisition faults or native macOS behavior.

## Source and remaining verification

The [source-archive entry point](../../../scripts/source-archive.sh) preserves the root manifest and lockfile, verifies registry Crossterm and published macOS TLS-verifier selections after extraction, and performs a fresh offline build check. The [bundle entry point](../../../scripts/release-bundle.sh) builds from that extraction and carries the checksum-matched Crossterm registry archive and original MIT text through its ordinary third-party path. The source archive contains no Crossterm vendor tree.

Keep PTY-loss, acquisition, output-fault and joined-input gates enabled. Before platform/release claims, run their native macOS owners on both supported architectures and inspect the exact distributed binary/source pair. Lost terminal input must never become a submitted Message. [Next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release) retains acquisition-fault, native platform and distribution verification.

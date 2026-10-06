# Finding: interactive PTY loss can strand Arany's input reader

Status: **published source selected; one intermittent Linux active-loss timeout remains unresolved, alongside native macOS and distribution gates**. Severity of the original defect: **high for local availability**.

## Boundary and current selection

Arany selects unmodified published `crossterm 0.29.0` with its registry checksum in [Cargo.lock](../../../Cargo.lock). The maintainer chose upstream maintenance over a checked-in vendor repair. Arany's [Unix input owner](../../../src/terminal/input/raw.rs) now owns bounded polling, EOF/permanent-error reporting and joined shutdown directly; canonical input has its own bounded reader. Crossterm remains responsible for terminal operations and dependency cursor queries. This does not establish that the upstream event backend is fixed or that every remaining cursor-fault timing is safe.

The published Unix Mio event backend ignores zero-byte reads and some permanent errors. [Upstream issue #1110](https://github.com/crossterm-rs/crossterm/issues/1110) and [repair PR #1116](https://github.com/crossterm-rs/crossterm/pull/1116) identify the upstream responsibility. Arany carries no path/git replacement or alternate event-backend dependency.

## Evidence

On 2026-10-06, the three Linux `lost_pty` product-process cases passed against registry 0.29.0 in 0.23 seconds: idle attached input, screen-reader setup input, and loss during an active synthetic Provider call. Their owners are [acquisition](../../../tests/session_run/active_terminal/acquisition.rs) and [active terminal](../../../tests/session_run/active_terminal.rs). They bound product exit, cancel the active local Provider socket, reject a fabricated answer and independently guard the exact test-owned product PID. This proves those Linux paths, not remote generation/billing cessation or native macOS behavior.

During subsequent concurrent optimized consolidation, the active-loss case once exceeded its three-second socket-closure observation deadline. The same case passed in isolation, in a finite twelve-case series with four concurrent invocations, and in the terminal group and later complete suite. These passing samples do not explain or clear the observed timeout. Its failure now reports the exact guarded product's process state/wait channel and a bounded redacted transcript. No threshold was raised, no test was disabled and no product workaround or restored vendor patch was introduced. The cause remains unverified; attribution to Crossterm would require failure-time evidence. [Next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release) owns that remaining verification.

The original 2026-10-01 implementation used Crossterm event polling and failed idle/active PTY-loss gates against the registry source. A scoped local Mio patch then made those gates pass. That earlier patched-source and archive evidence does not certify the current registry build. A read-only host inventory found 45 old processes accumulating CPU with deleted PTYs; their complete origins were not reconstructed. A later host reboot removed them without agent cleanup signals.

## Source and remaining verification

The [source-archive entry point](../../../scripts/source-archive.sh) preserves the root manifest and lockfile, verifies registry Crossterm and published macOS TLS-verifier selections after extraction, and performs a fresh offline build check. The [bundle entry point](../../../scripts/release-bundle.sh) builds from that extraction and carries the checksum-matched Crossterm registry archive and original MIT text through its ordinary third-party path. The source archive contains no Crossterm vendor tree.

Keep PTY-loss, acquisition, output-fault and joined-input gates enabled. Before platform/release claims, run their native macOS owners on both supported architectures and inspect the exact distributed binary/source pair. Lost terminal input must never become a submitted Message. Linux success does not clear arbitrary cursor acquisition/restoration timing, compiler provenance, or redistribution gates.

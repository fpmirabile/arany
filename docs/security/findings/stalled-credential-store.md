# Finding: a stalled OS credential store can strand setup

Status: **Linux synthetic read/write stalls mitigated; native beta release gate open**. Severity: **medium (local availability)**. No real account or credential was used in the stall reproductions.

## Evidence and boundary

On a bare attached start, saved-route discovery checks private metadata before OS-store access; without a configured account, it opens an unconfigured composer without probing the store. Selected keyring access follows the [credential contract](../../specs/credentials.md#selected-item-authorization). The pinned Linux path goes through `keyring 4.2.0` (`v1`), `zbus-secret-service-keyring-store 1.0.1`, `secret-service 5.2.0`, and `zbus 5.19.0`. Its blocking Secret Service connection can wait for a D-Bus peer that accepts a socket connection but does not complete authentication. Moving the call off the coordinator thread alone did not make setup responsive.

Tests: [`tests/session_run/setup.rs::stalled_credential_bus_does_not_strand_unconfigured_start`](../../../tests/session_run/setup.rs); [`src/cli/credentials/keyring_helper.rs::timed_out_write_is_uncertain_and_reaps_helper`](../../../src/cli/credentials/keyring_helper.rs); [`src/cli/credentials.rs::native_store_round_trips_an_isolated_synthetic_account`](../../../src/cli/credentials.rs).

Fake-bus and synthetic-child fixtures prove supervision and connection closure, not an OS-accepted stalled write or rollback. Isolated native Keychain fixtures are debug-only; human permission dialogs, optimized lifecycle and cross-user behavior remain separate gates.

This was a local availability failure, not evidence of key disclosure. The current fake-bus fixture exercises selected-account reads and preparation recovery; the separate synthetic child exercises write supervision, not the native service. A simple `tokio::time::timeout` around `spawn_blocking` would not stop the blocking operation. Killing the helper after a write timeout still cannot prove that a request already accepted by the OS service will not finish later; Arany keeps the keyring backend pinned and reports an uncertain failure, not cancellation or rollback. State and the OS store are not one transaction.

The pinned `keyring/v1` API exposes no operation deadline. Its Linux backend constructs the blocking Secret Service client itself, and `zbus::blocking::Connection::session()` runs the async connection to completion with its own executor. Zbus's configurable method-call timeout does not cover this observed pre-method authentication stall. The process boundary provides the deadline without modifying the backend. The helper uses a fixed service and admitted slot argument; the key travels only through bounded private stdin/stdout, never argv or the child environment. The helper has a fixed safe working directory, null stderr, and an allowlisted environment.

## Resolution and verification

The [timeout investigation](../../research/harness-credential-timeouts-2026-10.md#aranys-recorded-parent-deadline-miss) did not reproduce or establish the cause of the historical parent-observer miss; it is not an active follow-up.

Open gates: **isolated Linux keyring lifecycle** and **macOS optimized account isolation** in [next steps](../../../NEXT_STEPS.md#beta-2-and-broader-release); human permission dialogs remain in its final user-owned checklist.

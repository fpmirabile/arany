# Finding: a stalled OS credential store can strand setup

Status: **Linux synthetic stall mitigated; native beta release gate open**. Severity: **medium (local availability)**. No real account or credential was used in the reproduction.

## Evidence and boundary

On a bare attached start, [setup](../../../src/cli/attached/setup.rs) checks the saved account before creating a Session. The pinned Linux path goes through `keyring 4.2.0` (`v1`), `zbus-secret-service-keyring-store 1.0.1`, `secret-service 5.2.0`, and `zbus 5.19.0`. Its blocking Secret Service connection can wait for a D-Bus peer that accepts a socket connection but does not complete authentication. Moving the call off the coordinator thread alone did not make setup responsive.

The [product-process regression](../../../tests/session_run/setup.rs) starts the shipped `arany` in a private screen-reader PTY, points it at a test-owned Unix socket, accepts the connection, and deliberately sends no D-Bus reply. The parent now supervises a [same-binary helper](../../../src/cli/credentials/keyring_helper.rs), terminates it at five seconds, reaps it, and proceeds without creating a Session or State. The regression passes under a seven-second outer guard and observes EOF on the synthetic connection. The listener has no access to a real Secret Service or the fixed account slot.

An explicitly activated native Linux test saved and read a synthetic account through the shipped helper under a random slot, then deleted it. This proves one unlocked host-store roundtrip and the helper protocol, not a locked store, a private login bus, a stalled write, or macOS behavior.

This was a local availability failure, not evidence of key disclosure. The same helper supervises save and selected-account reads, but the fake-bus fixture exercises only first-run read. A simple `tokio::time::timeout` around `spawn_blocking` would not stop the blocking operation. Killing the helper after a write timeout still cannot prove that a request already accepted by the OS service will not finish later; Arany keeps the keyring backend pinned and reports an uncertain failure, not cancellation or rollback. State and the OS store are not one transaction.

The pinned `keyring/v1` API exposes no operation deadline. Its Linux backend constructs the blocking Secret Service client itself, and `zbus::blocking::Connection::session()` runs the async connection to completion with its own executor. Zbus's configurable method-call timeout does not cover this observed pre-method authentication stall. The process boundary provides the deadline without modifying the backend. The helper uses a fixed service and admitted slot argument; the key travels only through bounded private stdin/stdout, never argv or the child environment. The helper has a fixed safe working directory, null stderr, and an allowlisted environment.

## Resolution and verification

Before release, run isolated native Linux locked-store and stalled-write checks, a full setup save/reuse journey against a private unlockable service, and the macOS Keychain journey on a native runner. Verify that the helper preserves the chosen keyring behavior and error distinction, that secrets do not appear in argv, environment, user-visible output, or diagnostics, and that a timed-out write cannot be reported as rolled back. Keep the account-replacement lock through helper reaping. Do not use a real account or silently change the credential backend to make this gate green.

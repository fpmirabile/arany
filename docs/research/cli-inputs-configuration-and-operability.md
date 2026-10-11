# CLI admission and operability: design rationale

**Research baseline:** 2026-09-29.

This report retains reasons behind CLI admission and failure handling. Exact
commands, limits and supported access routes belong to their current owners:
[CLI rules](../../agents/cli.md), [terminal spec](../specs/terminal.md),
[credential spec](../specs/credentials.md), [Provider rules](../../agents/provider.md)
and [diagnostic spec](../specs/diagnostics.md).

## Explicit entry modes

Attached interaction and deterministic automation serve different callers.
Interactive selectors can ask a human to choose; machine output needs stable
channels and a bounded exit. Keeping interpretation explicit avoids treating a
headless prompt as a local command or silently choosing a Session or billing
route. The compiled CLI and slash registry own syntax and availability.

A durable Session belongs to Arany rather than a Provider conversation ID.
[Session rules](../../agents/session.md) own resume, fork and default selection;
this report does not keep another command grammar or Session lifecycle copy.

## Pin authority before reading project data

A repository can influence model input but cannot choose the credential,
endpoint, state root or capability used to read it. Trusted configuration is
resolved first. Explicit input snapshots make disclosure reproducible and keep
the model from obtaining an ambient path capability.

Path-string normalization or canonicalization alone cannot prevent a component
replacement between validation and open. Handle-relative no-follow traversal
keeps admission tied to the object actually read. Validation of the opened
object, bounded reading and exact-byte digests serve distinct purposes.
[Security rules](../../agents/security.md) own those constraints.

Platform directory conventions locate data; they do not prove privacy. A state
root and an OS-user account root also serve different scopes. Session overrides
must not create another implicit billing identity. [Credential scope](../specs/credentials.md)
and [Store rules](../../agents/store.md) own the checks and recovery behavior.

## Make network and billing choices visible

An ambient proxy, redirect or automatic fallback can change where a credential
or Workspace data goes. Exact admitted destinations and deliberate billing
selection are therefore part of authority, not HTTP convenience defaults.
A model catalog is useful for selection but cannot prove output guarantees or
admit a custom profile.

API-key access and consented ChatGPT-plan access have different cost guarantees.
[ChatGPT rules](../../agents/chatgpt.md) own account-bound consent and the weaker
local-only output bound; native API-key and custom profiles retain their own
admission contracts. Platform secret stores require explicit locked-service,
headless, replacement and recovery behavior rather than a generic promise to
use a keychain.

Automatic retries can repeat billable requests and require attempt accounting,
cancellation during backoff and an aggregate deadline. The retry policy lives
in [Provider rules](../../agents/provider.md); a retryable error classification
alone does not authorize another attempt or a billing fallback.

## Cancellation and failure honesty

A request can be cancelled locally after a remote Provider accepted it. A Tool
can change a Workspace before its final receipt is durable. Returning from a
wait therefore cannot imply remote rollback or the absence of an effect.
[Execution](../specs/execution.md) and [Tools](../specs/tools.md) own terminal
outcomes, uncertainty and cleanup; [terminal behavior](../specs/terminal.md)
owns input-state-dependent interruption and restoration.

Construct safe diagnostics from closed fields rather than formatting a rich
request error and trying to redact it afterward. Canonical Events, local
development diagnostics and OTLP have different content and retention roles.
Their current contracts must decide what crosses each boundary.

## Evidence and expansion

Use the real Engine, temporary filesystem and SQLite with the strict scripted
Provider for deterministic semantics. Product processes prove parsing and
channels; native PTYs prove terminal lifecycle. Live Provider and native OS
claims need their own evidence. [The testing strategy](./testing-strategy-for-rust-cli-harness.md)
explains this split; [testing rules](../../agents/testing.md) own procedures.

Add configuration only for a demonstrated need with an explicit precedence,
validated range and failure behavior. Enterprise proxy support, detached
Clients and direct remote telemetry would each introduce a new trust boundary.
They belong in a concrete proposal, not a standing table of hypothetical flags.
Open defects and verification debt belong in [next steps](../../NEXT_STEPS.md).

## Primary sources

- Rust: [terminal detection](https://doc.rust-lang.org/std/io/trait.IsTerminal.html), [process exit](https://doc.rust-lang.org/std/process/struct.ExitCode.html), [canonicalize](https://doc.rust-lang.org/std/fs/fn.canonicalize.html)
- [Capability filesystem design](https://github.com/bytecodealliance/cap-std/blob/main/README.md), [no-follow traversal](https://docs.rs/cap-fs-ext/latest/cap_fs_ext/trait.DirExt.html), [cap-std advisory](https://github.com/bytecodealliance/cap-std/security/advisories/GHSA-hxf5-99xg-86hw)
- Platform locations: [XDG](https://specifications.freedesktop.org/basedir/0.8/), [macOS](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/FileSystemProgrammingGuide/MacOSXDirectories/MacOSXDirectories.html), [Windows](https://learn.microsoft.com/windows/win32/shell/knownfolderid)
- Secret stores: [Keychain](https://developer.apple.com/documentation/security/keychain-services), [Secret Service](https://specifications.freedesktop.org/secret-service/latest-single/), [Windows credentials](https://learn.microsoft.com/windows/win32/secauthn/credentials-management)
- [Reqwest client controls](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html), [secrecy](https://docs.rs/secrecy/latest/secrecy/)
- Tokio: [graceful shutdown](https://tokio.rs/tokio/topics/shutdown), [cancellation tokens](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html), [Ctrl-C](https://docs.rs/tokio/latest/tokio/signal/fn.ctrl_c.html)
- [JSON Lines](https://jsonlines.org/), [NO_COLOR](https://no-color.org/)

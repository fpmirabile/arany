# Local development diagnostics

This contract owns activation, privacy, retention and safe failure metadata of the local development log. [Execution](./execution/README.md) owns outcome acceptance, which diagnostics never relax ([accepted outcomes](./execution/events-and-outcomes.md#accepted-outcomes)); [next steps](../../NEXT_STEPS.md) owns remaining checks; [diagnosing failures](../../agents/testing.md#diagnosing-failures) owns editing practice.

## Activation and records

Only the development build (`cargo run`) logs; the optimized build (`cargo run --release`) never does. Development chat and `exec` record runtime failures, without a flag, in `development.log` beneath their admitted Session State directory, outside the Workspace: `STATE_DIR/development.log` with `--state-dir`; on Linux `$XDG_STATE_HOME/arany/development.log`, or `$HOME/.local/state/arany/development.log` when unset; on macOS `~/Library/Application Support/dev.Arany.arany/development.log`. Help, version, `show`, the credential helper and the Tool Guard do not enable it, and errors before Session State admission stay on stderr without creating state.

Records cover failed Provider calls, unsuccessful Tools, failed Runs and compaction, typed Engine and filesystem errors, selected-account admission and fatal attached, `exec` or output exits, each as closed categories and numeric counters. A record never delays the Run. Expected command-validation feedback and successful calls are not logged; the log is not an exhaustive trace and cannot reconstruct a historical response or establish a live failure's cause.

## Context exhaustion

A primary `ToolContextFull` also records the bounded model identifier, window and budget figures and estimated per-component sizes, never component content, paths, Tool arguments or result values.

## Provider rejections

Provider rejections record the closed stage and cause, byte and event counters and the HTTP status when observed, never event content, rejected values or response text. Recording a rejection never changes the strict rejection, and adds no replay, retry or dispatch.

## Privacy

Only compiled categories, counters and the bounded model identifier on context exhaustion enter a record; never credentials, headers, raw errors, request or response bodies, account identities, prompts, answers, environment dumps, stack source locations or a failed remote event's raw message or code. There is no panic-payload or HTTP-body logger. Debug credential callers may report a validated native Keychain refusal category to stderr, never native error text, paths, identities or item contents; the credential result stays Locked without retry, unlock or fallback.

## File bounds

One no-follow, single-link, owner-checked `0600` file is capped at 256 KiB, each record at 24 KiB; at capacity the next record truncates older diagnostics. Busy locks, unsafe files or storage errors discard diagnostics without changing the Run result; the file is best-effort, not crash-durable, and its mode neither encrypts it nor excludes another same-user or privileged process. Records are never sent over the network or printed into chat, stdout, stderr or JSONL.

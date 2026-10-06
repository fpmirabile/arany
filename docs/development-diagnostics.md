# Local development diagnostics

`cargo run` uses this repository's development build; `cargo run --release` uses its optimized build. Development chat and `exec` record subscription failures in `development.log` beneath their admitted Session State directory, outside the Workspace. With an explicit `--state-dir`, that is `STATE_DIR/development.log`; Linux's ordinary default is `$XDG_STATE_HOME/arany/development.log`, or `$HOME/.local/state/arany/development.log` when that variable is absent. This Session path is distinct from the stable OS-user account root. The optimized build does not enable this log.

For a deliberate account retry, start the development executable normally:

```sh
cargo run --locked
```

An actual submitted task can consume plan usage. Arany does not retry, switch billing routes, read another harness's credentials or run a preliminary model check. Existing failed journals cannot reconstruct this new diagnostic detail: the log records only subsequent observed failures.

The log distinguishes transport/deadline errors, HTTP rejection, response encoding/length checks, EOF before `response.completed`, stream bounds, SSE framing/JSON/type rejection, `response.incomplete`, a remote failed event, and the local output cap. Completed-response rejection further distinguishes `ResponseEnvelope`, `ResponseModel`, `ResponseStatus`, `ResponseIdentifier`, `ResponseMessage`, `ResponseContent` and `ResponsePhase`. Final-message rejection has four closed causes:

- `ResponseMissingFinalMessage`: no assistant message qualified as final, including commentary-only output.
- `ResponseDuplicateFinalMessage`: two explicit `final_answer` messages were encountered.
- `ResponseAmbiguousFinalMessage`: multiple messages qualified as final and at least one had an absent/null phase.
- `ResponseLateCommentary`: commentary was encountered after a message qualified as final.

These categories report the first rejection encountered; the numeric shape counters below provide separate structural evidence. Invalid structured JSON is `OutcomeContract`, and missing/invalid final usage is `UsageContract`. Each record includes a local timestamp, process ID, measured received-byte/event counters, numeric HTTP status when observed, and a bounded local stack trace. Completed Run and compaction failures retain the stream's actual counters and status, rather than resetting them to zero. This is not an upstream stack trace or the full asynchronous request history.

Development streams also retain numeric `response_shape` counters: terminal message count, explicit commentary/final counts, absent/null-phase count, messages whose joined text matches the closed outcome or summary envelope, and message/final counts seen in `response.output_item.done`. This distinguishes a terminal response without messages from commentary-only output and from a message visible only in item events. A structured-envelope count is diagnostic shape, not accepted phase, scope, usage, bounds or Tool authority. No message text, unknown phase label, item ID or reasoning content enters these counters, and they never reconstruct or authorize an answer. Optimized builds omit this inspection and logging.

Only compiled categories and counters enter the record. Tokens, headers, raw errors, request/response bodies, model/account identities, prompts, answers and environment dumps are excluded. Stack source locations are omitted. A failed remote event's raw message/code is not logged. Ordinary expected Provider failures do not panic, so their captured stack is the rejection call site rather than a panic stack. There is no panic-payload or HTTP-body logger.

One no-follow, single-link, owner-checked `0600` file is capped at 256 KiB; each record is capped at 24 KiB. At capacity the next record truncates older diagnostics. Busy locks, unsafe files or storage errors discard diagnostics without changing the Run result; the file is best-effort and not crash-durable. File modes do not encrypt it or exclude another same-user or privileged process. Nothing is sent over the network or printed into chat, stdout, stderr or JSONL. Help/version, `show`, the credential helper and Tool Guard do not enable it.

The subscription transport always runs its one bounded SSE parser, so a complete valid stream is not rejected solely for an absent or misleading media label. Complete indexed `response.output_item.done` items can supply empty terminal output after matching created/completed response identity; populated terminal output must agree. Deltas cannot supply a result, and the received terminal shape counters describe the original closing event before reconciliation. Explicit assistant commentary is distinguished from the one final structured answer, whose ordered text parts are joined exactly. Malformed/refusal blocks, ambiguous finals and contradictory completion markers remain rejected. Older logs may retain `ContentType`, a coarse `ResponseContract` or `ResponseFinalMessage`, or reset counters; those records cannot identify the new detailed causes or reconstruct an upstream response. A historical `ResponseFinalMessage` cannot justify relaxing any one of these guards.

The user's live result remains unverified. Offline product tests prove the metadata compatibility case and that interrupted and incomplete streams remain rejected with distinct safe diagnostics in development; optimized processes keep the same rejection and channel/replay contract without logging. A new actual failure can narrow the investigation; it does not justify accepting partial output or bypassing identity, TLS, endpoint or usage checks.

# Local development diagnostics

This contract owns activation, privacy, retention and safe failure metadata. [Execution](./execution.md) owns outcome acceptance; [diagnostic rules](../../agents/diagnostics.md) retain instrumentation constraints.

`cargo run` uses this repository's development build; `cargo run --release` uses its optimized build. Development chat and `exec` automatically record runtime failures in `development.log` beneath their admitted Session State directory, outside the Workspace. No flag is required. With an explicit `--state-dir`, that is `STATE_DIR/development.log`; Linux's ordinary default is `$XDG_STATE_HOME/arany/development.log`, or `$HOME/.local/state/arany/development.log` when that variable is absent. This Session path is distinct from the stable OS-user account root. The optimized build does not enable this log.

Admission writes a `DiagnosticsEnabled` marker, so a new process can be distinguished from a log left by an older executable. Failed Provider calls across routes retain the closed phase/disposition/reason; unsuccessful Tools retain their compiled operation, disposition, receipt presence and stopping decision. Failed Runs and compaction, typed Engine Run/compaction errors, selected-account admission and fatal attached/exec/output exits also produce bounded records. Errors before normal Session State admission remain stderr-only: logging does not create state for invalid startup, help, read-only inspection or internal helpers. Expected command-validation feedback and successful calls are not an exhaustive event trace.

Rejected subscription Tool arguments additionally record `tool_arguments`: the compiled operation and numeric shape of its serialized size, path, digest, Read offset/limit, edit/replacement text and command arguments, with validity flags where available. This identifies the relevant field or bound without saving file names, digests, text, programs or argument values. It is metadata about a rejected proposal, not proof that the Tool was dispatched. No automatic replay or retry is introduced.

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

These categories report the first rejection encountered; the numeric shape counters below provide separate structural evidence. The general rejection is `OutcomeContract`; development Run diagnostics further distinguish malformed JSON (`OutcomeEncoding`), invalid envelope/field types (`OutcomeFields`), decoded Read limits outside 1..4096 are `OutcomeReadBounds`, and other typed Tool argument rejection is `OutcomeToolArguments`. Compiled subcategories identify invalid relative paths (`OutcomeToolPath`), file digests (`OutcomeToolDigest`), empty edit matches (`OutcomeToolEmptyEdit`), text or serialized-call bounds (`OutcomeToolTextSize` / `OutcomeToolArgumentSize`), and malformed command program names (`OutcomeToolProgram`). None stores the rejected value. These categories preserve the same strict rejection and never log rejected arguments or response text. Missing/invalid final usage is `UsageContract`. Each record includes a local timestamp, process ID, measured received-byte/event counters, numeric HTTP status when observed, and a bounded local stack trace. Completed Run and compaction failures retain the stream's actual counters and status, rather than resetting them to zero. This is not an upstream stack trace or the full asynchronous request history.

Development streams also retain numeric `response_shape` counters: terminal message count, explicit commentary/final counts, absent/null-phase count, messages whose joined text matches the closed outcome or summary envelope, and message/final counts seen in `response.output_item.done`. This distinguishes a terminal response without messages from commentary-only output and from a message visible only in item events. A structured-envelope count is diagnostic shape, not accepted phase, scope, usage, bounds or Tool authority. No message text, unknown phase label, item ID or reasoning content enters these counters, and they never reconstruct or authorize an answer. Optimized builds omit this inspection and logging.

Only compiled categories and counters enter the record. Tokens, headers, raw errors, request/response bodies, model/account identities, prompts, answers and environment dumps are excluded. Stack source locations are omitted. A failed remote event's raw message/code is not logged. Ordinary expected Provider failures do not panic, so their captured stack is the rejection call site rather than a panic stack. There is no panic-payload or HTTP-body logger.

One no-follow, single-link, owner-checked `0600` file is capped at 256 KiB; each record is capped at 24 KiB. At capacity the next record truncates older diagnostics. Busy locks, unsafe files or storage errors discard diagnostics without changing the Run result; the file is best-effort and not crash-durable. File modes do not encrypt it or exclude another same-user or privileged process. Nothing is sent over the network or printed into chat, stdout, stderr or JSONL. Help/version, `show`, the credential helper and Tool Guard do not enable it.

Subscription media compatibility, complete item reconciliation and strict final-outcome acceptance belong to [execution](./execution.md#accepted-outcomes). Diagnostic inspection cannot relax those guards. Older log categories or reset counters cannot reconstruct a historical response or establish the cause of a live failure. Dated synthetic/live results and remaining checks belong to [next steps](../../NEXT_STEPS.md).

## Acceptance and evidence owners

| Scenario | Required observation | Owner |
| --- | --- | --- |
| Normal debug admission, then a rejected custom-provider Run | Activation, failed Provider phase/disposition and failed Run appear in the private log; exact task channels and closed replay retain their contract | [Custom product process journey](../../tests/session_run/custom.rs) |
| Subscription stream/outcome rejection | One subscription rejection record preserves actual counters alongside Engine failure facts; credentials, conversation and upstream canaries are absent | [Offline HTTPS subscription journey](../../tests/session_run/setup/offline_https/chatgpt/checked_turn.rs) |
| Malformed typed Tool argument | Strict decoder still rejects; compiled operation and numeric argument shape identify bounds without field values | [Native wire rejection corpus](../../src/provider/openai/wire/tests.rs) |
| Tool uncertainty or Engine filesystem error | Compiled operation/receipt/stopping facts or closed error kind are logged; nested raw errors and Tool output are omitted | [Diagnostic privacy corpus](../../src/diagnostics.rs) |
| Log links, unsafe permissions, contention or capacity | Checked writer refuses unsafe aliases; bounded best-effort logging never changes task acceptance | [Diagnostic privacy and state writer](../../src/diagnostics.rs), [state writer](../../src/store/state.rs) |
| Optimized process or pre-admission inspection | No enabled diagnostic file; existing stdout/stderr and no-state contracts remain intact | [Custom product process journey](../../tests/session_run/custom.rs), [offline subscription journey](../../tests/session_run/setup/offline_https/chatgpt/checked_turn.rs), existing CLI admission corpus |

# Local development diagnostics

This contract owns activation, privacy, retention and safe failure metadata. [Execution](./execution/README.md) owns outcome acceptance; [next steps](../../NEXT_STEPS.md) owns remaining checks. Editing practice: [diagnosing failures](../../agents/testing.md#diagnosing-failures).

`provider::diagnostics` builds Provider failure, stream, response-shape, Tool-argument-shape and context-exhaustion records, which Provider producers and the Engine submit as inert bounded records to the shared sink. Root diagnostics owns activation, locking, sanitization, record bounds, file admission and append.

## Activation and records

`cargo run` uses the development build; `cargo run --release` uses the optimized build, which never enables this log. Development chat and `exec` record runtime failures in `development.log` beneath their admitted Session State directory, outside the Workspace, without a flag. With `--state-dir` that is `STATE_DIR/development.log`. The Linux default is `$XDG_STATE_HOME/arany/development.log`, or `$HOME/.local/state/arany/development.log` when that variable is unset; the macOS default is `~/Library/Application Support/dev.Arany.arany/development.log`. This Session path is distinct from the OS-user account root.

Admission writes a `DiagnosticsEnabled` marker, so a new process is distinguishable from a log left by an older executable. Records cover:

- failed Provider calls on every route: the closed phase, disposition and reason, plus, for an Engine proposal refusal, the compiled operation and closed cause;
- unsuccessful Tools: compiled operation, disposition, receipt presence, stopping decision and closed error variant;
- an unguarded re-run that fails before dispatch: site `unguarded` and its failure stage, never its argv;
- failed Runs with their committed closed failure cause, failed compaction, typed Engine Run, compaction and filesystem errors, selected-account admission and fatal attached, `exec` or output exits.

A Journal subscriber thread records failed Provider-call, Tool, Run and compaction Events, so their stack is the subscriber's, not the append site's. A record never delays the Run; the Engine's close waits for the subscriber. The subscriber keeps at most 256 unread Events; beyond that the oldest are dropped unrecorded and one `journal resync missed=N` record states how many. Errors before Session State admission stay on stderr: invalid startup, help, read-only inspection and internal helpers create no state. Expected command-validation feedback and successful calls are not logged; the log is not an exhaustive trace.

## Context exhaustion

A primary `ToolContextFull` also records a bounded model identifier, optional trusted window, effective content ceiling, Tool allowance, output-token cap and estimated content bytes and tokens (three bytes per token, not a tokenizer or wire-body measure). Fixed component labels sorted by size cover objective, instructions, includes, history, summary, Tools, pending turn, images, steering and Goal metadata; catalog bytes and the largest live turn or result are separate counters. No component content, path, Tool argument or result value is recorded. This Engine-side record complements the subscriber's failure record and covers initial Skill admission and primary continuation exhaustion.

## Provider rejections

Rejected subscription and Anthropic Messages Tool arguments also record `tool_arguments`: the compiled operation and the numeric shape of its serialized size, path, digest, Read offset and limit, edit and replacement text and command arguments, with validity flags where available. It identifies the field or bound without saving file names, digests, text, programs or values. It is metadata about a rejected proposal, not proof of dispatch, and adds no replay or retry.

Shared SSE framing rejects a limit with `stream reason=StreamProtocol stage=StreamLimit` and byte and event counters on every route, without event content. Rejected Anthropic Messages streams and decoded responses also record `messages stage=<closed stage> bytes= events= http_status=`. The log distinguishes transport or deadline errors, HTTP rejection, response encoding and length checks, EOF before completion, stream bounds, SSE framing, JSON or type rejection, remote failure, response-content rejection and the local output cap. Completed-response rejection further distinguishes `ResponseEnvelope`, `ResponseModel`, `ResponseStatus`, `ResponseIdentifier`, `ResponseMessage`, `ResponseContent` and `ResponsePhase` where the adapter checks them. Final-message rejection has four closed causes:

- `ResponseMissingFinalMessage`: no assistant message qualified as final, including commentary-only output.
- `ResponseDuplicateFinalMessage`: two explicit `final_answer` messages.
- `ResponseAmbiguousFinalMessage`: several messages qualified as final and at least one had an absent or null phase.
- `ResponseLateCommentary`: commentary after a message qualified as final.

These categories report the first rejection encountered. The general outcome rejection is `OutcomeContract`; Run diagnostics further distinguish malformed JSON (`OutcomeEncoding`), invalid envelope or field types (`OutcomeFields`), decoded Read limits outside 1..4096 (`OutcomeReadBounds`) and other typed Tool argument rejection (`OutcomeToolArguments`). Compiled subcategories identify invalid relative paths (`OutcomeToolPath`), file digests (`OutcomeToolDigest`), empty edit matches (`OutcomeToolEmptyEdit`), text or serialized-call bounds (`OutcomeToolTextSize`, `OutcomeToolArgumentSize`) and malformed program names (`OutcomeToolProgram`). Missing or invalid final usage is `UsageContract`. Every category keeps the same strict rejection and never stores the rejected value or response text.

Each record has a local timestamp, process ID, measured received-byte and event counters, numeric HTTP status when observed and a bounded local stack trace; it is not an upstream stack or the full asynchronous request history. Measured counters carry through completed Run and compaction decoding: a completed stream is never recorded as zero counters, and an outcome rejection is never collapsed into a response rejection.

Development streams also keep numeric `response_shape` counters: terminal message count, explicit commentary and final counts, absent or null phase count, and message and final counts seen in `response.output_item.done`. They separate a terminal response without messages from commentary-only output and from a message visible only in item events. They hold no message text, unknown phase label, item ID or reasoning content, establish no accepted phase, scope, usage, bound or Tool authority, and never reconstruct or authorize an answer.

## Privacy

Only compiled categories, counters and the bounded model identifier on context exhaustion enter a record. Credentials, headers, raw errors, request and response bodies, account identities, prompts, answers, environment dumps, stack source locations and a failed remote event's raw message or code are excluded. Expected Provider failures do not panic, so their stack is the rejection site. There is no panic-payload or HTTP-body logger.

Debug credential callers also report a validated native Keychain refusal category to stderr, including before Session admission. The bounded helper reply admits only compiled `ERR_SEC_*` constant names for storage-access refusals; malformed or unknown payloads reject. Native error text, paths, identities and item contents are never forwarded. Optimized callers omit this diagnostic, and the credential result stays Locked without retry, unlock or fallback.

## File bounds

One no-follow, single-link, owner-checked `0600` file is capped at 256 KiB; each record is capped at 24 KiB. At capacity the next record truncates older diagnostics. Busy locks, unsafe files or storage errors discard diagnostics without changing the Run result; the file is best-effort and not crash-durable. File modes neither encrypt it nor exclude another same-user or privileged process. Records are never sent over the network or printed into chat, stdout, stderr or JSONL. Help, version, `show`, the credential helper and the Tool Guard do not enable it.

Subscription media compatibility, item reconciliation and strict final-outcome acceptance belong to [execution](./execution/events-and-outcomes.md#accepted-outcomes); diagnostics cannot relax them. Log records cannot reconstruct a historical response or establish the cause of a live failure.

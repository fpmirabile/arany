# Serve protocol and schema

## Scope and related contracts

This contract owns the JSON-RPC vocabulary, codec and JSON Schema export for
`arany serve --stdio`. Schema export is available; the stdio server and method
handlers are accepted but unimplemented under [NEXT_STEPS item 8](../../NEXT_STEPS.md#pursue-in-order).
Exported types describe that reserved surface, not running server capabilities.

[Session](./session.md) owns durable Session identity and replay,
[execution](./execution.md) owns committed Runs and Events,
[Goals](./goals.md) owns budgets and recovery, and [Tools](./tools.md) owns
authority and one-use approvals. [Terminal](./terminal.md) owns `exec` and
`show` output. Protocol serialization adds no authority or Engine policy.

## Framing and envelope

Messages are UTF-8 JSON objects, one per newline-terminated line. Batches,
non-objects, malformed JSON, non-integer numeric IDs and ambiguous envelopes
are rejected. Encoded strings escape embedded newlines and control bytes.
Outbound envelopes always include `"jsonrpc":"2.0"`; inbound envelopes accept
its absence, matching Codex app-server clients, but reject any other value,
including null. Request IDs are strings or signed 64-bit integers; only error
responses can carry a null ID. Server request IDs use a separate integer
namespace starting at 1.

Requests carry `id`, `method` and optional `params`; notifications omit `id`.
Responses carry `id` and `result`; error responses carry `id` and
`error: {code, message, data?}`. `data` contains a closed camelCase `reason`.
Envelope fields and typed params/results reject unknown fields, except
`initialize.capabilities`, whose unknown keys are ignored for feature detection.

Protocol fields use camelCase. Embedded journal vocabulary (`EventLine`, Event
payloads, Tool calls and intents) retains its existing snake_case bytes.
`EventLine.payload` is an open object: exporting a schema does not redefine the
journal format. JSON is data; clients must render untrusted content inertly.

### Error model

These codes and reasons reserve the server's error contract. The codec reports
`Parse` or `TooLarge`; a server adapter supplies the corresponding response.

| Code | Meaning | `data.reason` |
|---|---|---|
| `-32700` | Malformed message; null ID | Absent |
| `-32600` | Invalid protocol state or oversized message | `notInitialized`, `alreadyInitialized`, `messageTooLarge`, `busy`, `threadNotLoaded`, `threadLimit`, `turnNotActive`, `serverShuttingDown` |
| `-32601` | Unknown method | Absent |
| `-32602` | Invalid parameters or unsupported permission mode | `invalidParams`, `unsupportedPermissionMode`; reserved steering refusals `runFull`, `empty`, `tooLarge`, `unavailable` |
| `-32603` | Internal channel or serialization failure | Absent |
| `-32000` | Engine admission failure | EngineError variant in camelCase; the closed vocabulary is exported as `$defs.Reason` |
| `-32001` | Provider selection or routing failure | `providerUnavailable`, `providerRejected`, `invalidSelection` |

A failed accepted turn is represented by `turn/completed` with status `failed`
and compiled recovery guidance in `error.message`; cancellation is
`interrupted`. An unknown Tool effect remains `uncertain`, never successful or
merely failed, preserving the honest-uncertainty guarantee.

## Methods

| CLI entry | Observable behavior |
|---|---|
| `arany generate-schema` | Writes one pretty-printed JSON Schema document plus a final newline to stdout; stderr is empty on success; no State, credentials or Engine are opened. |
| `arany generate-schema --out DIR` | Creates DIR as needed and atomically replaces `DIR/arany-protocol.schema.json` using an exclusively created temporary file in DIR; success leaves stdout and stderr empty. Refuses an existing symlink or non-file target. Failure exits 1 with a compiled `error: …` diagnostic on stderr. Success exits 0. |

The document is titled `arany-protocol`, carries schema artifact `version: "1"`,
uses JSON Schema draft 2020-12 and sorted object keys, and contains `$defs`,
`methods`, `notifications` and `serverRequests`. Registry entries reference
schemas in `$defs`; the document root validates JSON-RPC envelopes. Consumers
validate method-specific params/results through the corresponding registry
entry with the same `$defs`. Output is deterministic for a fixed contract.

The reserved method registry contains `initialize`, `thread/start`,
`thread/resume`, `turn/start` and `turn/interrupt`. A thread maps to a durable
Session and a turn to a Run with a separate Goal ID. `thread/start` must commit
`SessionStarted` before responding. Method execution and ordering evidence
belong to the remaining server work, not schema generation. Permission modes
use Claude Code spellings `default`, `acceptEdits` and `auto`; `plan` and
`dontAsk` are rejected until the [permission-rule work](../../NEXT_STEPS.md#pursue-in-order).

## Notifications and items

The reserved notification registry contains `initialized`, `thread/started`,
`turn/started`, `turn/completed`, `item/started`, `item/completed` and
`events/resync`. Item tags are `userMessage`, `agentMessage`,
`commandExecution`, `fileChange`, `mcpToolCall` and `toolCall`. Tool item status
is `inProgress`, `completed`, `failed`, `declined` or `uncertain`;
`disposition` preserves the journaled value when available. The additional
`uncertain` status diverges from Codex to preserve an unknown effect outcome.
Times represent committed Event timestamps, not a presentation clock.

## Server requests

The schema reserves `item/commandExecution/requestApproval`,
`item/fileChange/requestApproval` and `item/tool/requestApproval`, with the
journaled intent, its identity and expiry, and sanitized display lines. Decisions
are `accept`, `decline` or `cancel`. `acceptForSession` is excluded because an
immutable one-use intent cannot become a remembered permission rule. Dispatch
and approval handling are not implemented by schema export.

## Limits and compatibility

| Bound | Limit and overflow |
|---|---|
| Encoded or decoded line, including newline when present | 1 MiB; codec returns `TooLarge` before decoding and stops outbound serialization at the bound |
| `initialize.clientInfo` strings | 256 UTF-8 bytes each; invalid params |
| Notification opt-outs | At most 32; invalid params |
| Turn input | At most one text of 64 KiB; base64-encoded images follow the [terminal image contract](./terminal.md#paste-and-attachments), including its aggregate decoded-byte limit; invalid params |
| Pending inbound requests (reserved server) | 64; pipe backpressure |
| Outbound queue (reserved server) | 256; follower waits and Journal lag is reported through `events/resync` |
| Loaded threads / active turns / pending approval requests (reserved server) | 64 / 1 / 1; `threadLimit` / `busy` / sequential approval |
| Event page (future subscription) | 1,000; explicit continuation |
| Shutdown grace (reserved server) | 3 seconds after cancellation; unsuccessful exit with recovery guidance on timeout |

A future stdio reader must discard an oversized line through its newline
without retaining the overflow and return `-32600`, null ID,
`reason: "messageTooLarge"`. Malformed input returns `-32700`, null ID. A
rejected line must not consume the next valid message. No stdio reader runs
in this slice.

Schema string lengths are character constraints; typed admission additionally
checks the stated UTF-8 byte limits, text/image counts and base64 data. Paths
from clients are absolute; schema validation never replaces Workspace admission,
Provider selection or Guard enforcement. Embedded open objects are journal data,
not executable configuration. No socket listener, authentication, multi-user
isolation, encryption or native platform-support claim is introduced.

The protocol uses capability strings for feature detection; schema artifact
versioning is separate. The reserved initial strings are `threads_v1`,
`turns_v1`, `items_v1`, `interrupt_receipt_v1`. Approval capability advertisement
waits for an implemented approval handler. Breaking wire changes require a new
capability string and at most one release of deprecation compatibility during
pre-beta; stored formats retain their existing no-migration contract.

## Acceptance scenarios and evidence owners

| Scenario | Required observation | Evidence owner |
|---|---|---|
| Stable schema contract | Byte-identical pretty JSON and final newline; intentional golden changes only | [Protocol tests](../../crates/arany-cli/src/protocol/tests.rs), [schema golden](../../crates/arany-cli/tests/fixtures/serve/schema.json) |
| Envelope conformance and bounds | Both version forms accepted; emitted version present; all four envelopes round-trip; malformed, ambiguous and oversized lines rejected; exact 1 MiB boundary accepted | [Protocol codec table](../../crates/arany-cli/src/protocol/tests.rs) |
| Typed conformance | Hand-reviewed method, item, notification and approval examples validate against the registered schema; unknown fields, unsupported modes and input overflow rejected | [Protocol conformance corpus](../../crates/arany-cli/src/protocol/tests.rs) |
| CLI export and refusal | Fresh process exits 0 with exact stdout or file bytes; repeat export replaces safely; symlink/non-file refusal exits 1 with exact diagnostic and preserves external bytes | [Schema process test](../../crates/arany-cli/tests/generate_schema.rs) |

Server execution, journal reopen, interruption, approval dispatch and transport
recovery are acceptance work for [NEXT_STEPS item 8](../../NEXT_STEPS.md#pursue-in-order).

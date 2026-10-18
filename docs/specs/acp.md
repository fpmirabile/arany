# Agent Client Protocol

## Scope and related contracts

This contract owns `arany acp`, an ACP v1 subprocess on stdio. The current
surface is core text turns only. [Serve](./serve.md) owns the shared Server
loop, admission, committed projections, cancellation and shutdown;
[Session](./session.md), [Goals](./goals.md), [execution](./execution.md) and
[Tools](./tools.md) retain Engine policy. ACP translates the serve vocabulary;
it adds no execution runtime or library protocol dependency.

The subprocess and authentication shapes follow Goose; local execution rather
than editor filesystem/terminal delegation follows codex-acp. Unlike peers'
Provider deltas, Arany sends only the final committed answer. This preserves
journal-first truth and avoids chunks that cannot be retracted after a failed
Provider validation. The hand-written wire subset is checked against the
[pinned upstream v1 schema fixture](../../crates/arany-cli/tests/fixtures/acp/README.md).

Tools, images, audio, embedded resources, history loading, session
discovery/closure, modes, model selection and Goal commands are unavailable.
Their remaining slices belong to
[NEXT_STEPS item 13](../../NEXT_STEPS.md#pursue-in-order). Unadvertised requests
fail with method-not-found; unsupported prompt blocks fail invalid-params.

## Behavior

`arany acp --provider PROVIDER --model MODEL [--effort LEVEL]` pins the same
selection path as `exec`; `--profile NAME` selects a private roster and refuses
a conflicting explicit tuple. Collaboration and maximum children use `exec`
defaults and bounds. `--state-dir` selects State. Tools are off; `--tools` is an
unknown flag, refused before State admission with the standard invalid-arguments
diagnostic and exit 1. No permission channel is exposed, including human-only asks. Neither client capabilities nor `_meta` supply authority.

TTY stdin or stdout is refused before State admission, exit 2 with
`error: acp requires piped stdin and stdout`. Authority configuration, State,
signal handling and the same-binary Guard identity are fixed before stdin is
consumed. No terminal UI is initialized. Stdout contains only UTF-8 JSON-RPC
2.0 objects, one per newline. Diagnostics go to stderr; ordinary success and
missing credentials leave stderr empty. A selected roster prints its existing
bounded manifest on stderr before credential access.

| Method | Input → result | Admission and ordering |
|---|---|---|
| `initialize` | `{protocolVersion,clientInfo?,clientCapabilities?}` → `{protocolVersion:1,agentInfo,agentCapabilities,authMethods}` | First request; a repeat is `alreadyInitialized`. The client version is a u16 integer; unsupported integer versions negotiate down/up to 1, and the client disconnects if it cannot speak 1. Malformed versions fail `invalidParams`. Client info fields pass through serve's 256-byte checks. |
| `authenticate` | `{methodId:"arany-setup"}` → `{}` | Re-probes the pinned selection without inference. Exactly one agent auth method directs the user to `arany --setup` in a terminal. Missing credentials give `auth_required`; another method ID is invalid. No credential is accepted or returned. |
| `session/new` | `{cwd,mcpServers,additionalDirectories?}` → `{sessionId}` | Maps to typed `thread/start` with the CLI tuple and `tools:false`. Absolute cwd, Workspace identity and State overlap use serve admission. Nonempty additional directories fail. Editor MCP servers are ignored with one fixed stderr notice per connection; no server is launched or contacted. Success commits Session creation before replying and counts toward serve's loaded-thread limit. |
| `session/prompt` | `{sessionId,prompt:[{type:"text",text}\|{type:"resource_link",name,uri}]}` → `{stopReason}` | Requires available selection and a loaded Session. Concatenates blocks in order without separators; each resource link becomes the literal mention `[@name](uri)`. The URI is never fetched, opened or turned into authority. Maps to `turn/start`, starting one ordinary Goal. One prompt per process; a concurrent prompt fails `busy`, without queueing or execution. |
| `session/cancel` | Notification `{sessionId}` | Cancels that Session's active prompt through `turn/interrupt`. Before the turn ID arrives, cancellation is latched and forwarded once. Repeated/unknown cancellations do nothing. |
| `$/cancel_request` | Notification `{requestId}` | Same cancellation when the ID names the active prompt; unknown IDs do nothing. |

Capabilities declare `loadSession:false`, image/audio/embeddedContext false,
MCP HTTP/SSE false, and empty session capabilities. Client filesystem and
terminal offers are ignored. Arany never sends `fs/*`, `terminal/*` or
`elicitation/*`, and never connects editor-supplied MCP servers.

A committed serve `item/completed` agentMessage produces exactly one
`session/update` with `update.sessionUpdate:"agent_message_chunk"` and
`content:{type:"text",text}`. User input is not echoed. Provider previews,
commentary and uncommitted display text are absent. The prompt response follows
all its queued updates. Text passes through as model markdown; the editor owns
inert rendering. This transport makes no claim about editor auto-fetching.

On a tap gap, or a completed turn missing its answer, the translator reconciles
committed `MessageCommitted` facts through serve `events/subscribe`, paging
from the last reconciled Session sequence and filtering by the active Run.
Already sent answers are suppressed. Event paging preserves the loaded thread's
pinned settings, whereas `thread/resume` would re-admit and replace them.
A replay refusal produces one fixed diagnostic and
`result._meta["arany.incompleteUpdates"]:true`. During EOF/stop shutdown no new
replay work is started; missing updates carry the same marker. If serve closes
before a recovery reply, the committed completion still receives a prompt
response with this marker. A new process never resumes or retries a Goal implicitly.

| Committed outcome | Prompt result |
|---|---|
| Run finished and Goal completed | `stopReason:"end_turn"` |
| Client cancellation or process stop | `stopReason:"cancelled"`, including a teardown error after cancellation |
| Failed Run or hook stop | `-32603` with compiled recovery guidance and `data.reason:"turnFailed"` or `"stoppedByHook"` |
| Budget stop | `-32603 goalBudgetExhausted`; token/action stop reasons await a closed serve reason in the remaining slices |

Failure is never `end_turn`; `refusal` is unused because journal context is
retained. A submission without a committed terminal Run retains serve's fatal
Session-history recovery behavior. EOF, input/output failure and SIGINT,
SIGTERM or SIGHUP cancel work and close the Engine within serve's shared grace.
Clean EOF exits 0; transport/storage errors or grace expiry exit 1.

### Errors and envelopes

Envelopes require `jsonrpc:"2.0"`, reject batches, unknown envelope fields,
non-integer IDs and ambiguous shapes. IDs are signed i64 integers or strings
of at most 256 bytes; replies echo valid IDs unchanged. An oversized ID gets a
null-ID invalid request. Unknown params and `_meta` fields are ignored for
forward compatibility. Unknown notifications and unsolicited responses are
ignored and cannot authorize anything.

Two deliberate compatibility divergences retain typed admission: the peer Rust
SDK interprets a string `protocolVersion` as v0 and answers 1, whereas Arany
requires a u16 integer and refuses strings with `-32602 invalidParams`. The ACP
schema allows null request IDs; Arany retains serve's integer/string request IDs
and refuses null request IDs with `-32700 parseError`. Null IDs remain valid in
error responses.

| Code | Meaning and reasons |
|---|---|
| `-32700` | Malformed JSON or envelope, including a null request ID; `parseError` |
| `-32600` | `notInitialized`, `alreadyInitialized`, `messageTooLarge`, `invalidRequest`, `busy`, `threadLimit` |
| `-32601` | Unsupported method |
| `-32602` | Invalid params or unsupported content; `invalidParams` |
| `-32000` | `auth_required`, with terminal setup guidance |
| `-32002` | Unknown/unloaded Session after authentication; `resource_not_found` |
| `-32603` | Serve Engine/selection failures retain their closed reason; accepted turn failures use the outcome table above |

Serve `providerUnavailable` selection failures become `auth_required`. Other
serve `-32000`/`-32001` errors become `-32603`, preserving their reason and compiled
message. Initialization succeeds without credentials and creates no Session.

## Limits and compatibility

Shared bounds and their Engine behavior belong to [serve limits](./serve.md#limits-and-compatibility).

| ACP boundary | Bound and overflow |
|---|---|
| Incoming/outgoing line including newline | Serve's 1 MiB. Incoming overflow is discarded through newline, then null-ID `-32600 messageTooLarge`; malformed JSON gets `-32700`. The next line remains usable. |
| Reader buffer / serve inbound | 64 KiB / 64 messages with backpressure. JSON depth uses serde_json's recursion bound. |
| Serve outbound / ACP outbound | 256 messages each. Projection waits for ACP capacity independently of input/cancel. If immediate replies fill ACP output, input fails closed and starts shutdown rather than holding cancellation behind stdout. |
| Pending translations | 64, with 62 available to client requests and two reserved for control/reconciliation; excess requests fail `busy`. Integer internal IDs are checked and never reused. |
| Sessions / active prompt | Serve's 64 / 1, with `threadLimit` / `busy`, no eviction. Restart is needed after capacity is exhausted until session closure ships. |
| Prompt blocks / aggregate text after resource-link conversion | 64 / 64 KiB UTF-8, including mention syntax; excess, empty or whitespace-only prompts fail `invalidParams`. |
| Oversized answer projection | Cut at a UTF-8 boundary to at most 128 KiB text plus `[truncated; see arany show SESSION_ID]`; worst-case JSON escaping still fits the line cap. Other oversized projections fail closed. |
| Reconciliation | Serve's bounded Event pages and Session Event cap; no retained history beyond the current page, one active answer and one cursor per Session. |
| Shutdown | Serve's total three-second grace includes both output queues and cancellation/Engine closure. Timeout reports `Run may be interrupted; check Session history`. |

Linux and macOS share the same Unix pipes, descriptor cloning and signal path;
there is no shell or service-manager dependency. Admission remains canonical
and path text never grants authority. Native editor support requires separate
user evidence in NEXT_STEPS. No socket listener, editor authentication,
multi-user isolation, encryption, new Provider call/retry path, broader child
authority or credential channel is introduced. Existing Goal budgets, private
State storage and all per-action resource limits remain unchanged.

## Acceptance scenarios and evidence owners

| Scenario | Required observation | Evidence owner |
|---|---|---|
| Core committed turn | Canonical transcript validates against pinned upstream v1 schema; update equals reopened MessageCommitted and precedes end_turn; durable RunFinished and reduced Session agree | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs), [golden](../../crates/arany-cli/tests/fixtures/acp/basic.jsonl) |
| Negotiation and unavailable auth | Integer version negotiation, malformed versions, duplicate init, unknown auth method, unauthenticated prompt, unknown Session and unsupported method refusals | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Cancellation and capacity | Busy refusal runs no second prompt; session and request cancellation, pre-accept latch, EOF and stalled output produce bounded interruption without invented success | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Hostile input and recovery | Oversized/malformed lines recover; resource links remain literal mentions within block/UTF-8 bounds; relative cwd, additional directories, unsupported/oversized prompts and ambiguous envelopes refuse; offered fs/terminal and editor MCP never execute; EOF during recovery preserves the committed completion with incompleteUpdates | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Shipped command | TTY refusal exits 2 and unknown `--tools` exits 1 before State; cleared credentials initialize successfully, auth/new fail auth_required, no Session, EOF exits 0 with empty stderr and schema-valid stdout | [Session process corpus](../../crates/arany-cli/tests/session_run.rs) |

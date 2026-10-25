# Agent Client Protocol

## Scope and related contracts

This contract owns `arany acp`, an ACP v1 subprocess on stdio. The current
surface includes core turns, guarded Tools, one-use permissions, rich prompts,
stored Session loading/resumption, closure/discovery, live approval modes,
budget stop reasons and committed Goal plans.
[Serve](./serve.md) owns the shared Server loop, admission, committed projections, cancellation and shutdown;
[Session](./session.md), [Goals](./goals.md), [execution](./execution.md) and
[Tools](./tools.md) retain Engine policy. ACP translates the serve vocabulary;
it adds no execution runtime or library protocol dependency.

The subprocess and authentication shapes follow Goose; local execution rather
than editor filesystem/terminal delegation follows codex-acp. Unlike peers'
Provider deltas, Arany sends only the final committed answer. This preserves
journal-first truth and avoids chunks that cannot be retracted after a failed
Provider validation. The hand-written wire subset is checked against the
[pinned upstream v1 schema fixture](../../crates/arany-cli/tests/fixtures/acp/README.md).

Audio and model selection are unavailable. Goal pause/resume slash commands
and `available_commands_update` are deliberately omitted under the accepted
default; add them only if dogfooding requires them. A slash-prefixed prompt
remains ordinary user text, never a Goal control. Unadvertised requests
fail with method-not-found; unsupported prompt blocks fail invalid-params.

## Behavior

`arany acp --provider PROVIDER --model MODEL [--effort LEVEL]` pins the same
selection path as `exec`; `--profile NAME` selects a private roster and refuses
a conflicting explicit tuple. Collaboration and maximum children use `exec`
defaults and bounds. `--state-dir` selects State. Tools stay off unless `--tools`
opts into the existing private Tool grant and serve admission, including native
Guard availability. No client capability or `_meta` supplies authority. Human-only
approvals are unavailable under the [Tools contract](./tools.md#modes-and-approval-decisions). H1 permits only eligible admitted user-owned host-prefix allows in default/dontAsk; unresolved host asks deny immediately without a permission request. Auto/acceptEdits still require a fresh human and therefore refuse host commands here.

TTY stdin or stdout is refused before State admission, exit 2 with
`error: acp requires piped stdin and stdout`. Authority configuration, State,
signal handling and the same-binary Guard identity are fixed before stdin is
consumed. No terminal UI is initialized. Stdout contains only UTF-8 JSON-RPC
2.0 objects, one per newline. Diagnostics go to stderr; ordinary success and
missing credentials leave stderr empty. A selected roster prints its existing
bounded manifest on stderr before credential access.

The editor launch directory is not a Workspace. Startup loads and validates a
selected private roster without Workspace admission. Each `session/new.cwd`
passes the full profile/Provider, State-overlap and Workspace-identity admission
before Session creation. Loading and resuming repeat that admission against the
stored Session. Authentication checks current credentials without a
Workspace or inference; ChatGPT token refresh and workspace-dependent account
admission wait for Session new/load/resume. Authentication alone grants no Run authority.

| Method | Input → result | Admission and ordering |
|---|---|---|
| `initialize` | `{protocolVersion,clientInfo?,clientCapabilities?}` → `{protocolVersion:1,agentInfo,agentCapabilities,authMethods}` | First request; a repeat is `alreadyInitialized`. The client version is a u16 integer; unsupported integer versions negotiate down/up to 1, and the client disconnects if it cannot speak 1. Malformed versions fail `invalidParams`. Client info fields pass through serve's 256-byte checks. |
| `authenticate` | `{methodId:"arany-setup"}` → `{}` | Re-probes credentials for the pinned selection without inference or Workspace admission. Exactly one agent auth method directs the user to `arany --setup` in a terminal. Missing credentials give `auth_required`; another method ID is invalid. No credential is accepted or returned. |
| `session/new` | `{cwd,mcpServers,additionalDirectories?}` → `{sessionId,modes,configOptions}` | Maps to typed `thread/start` with the CLI tuple and CLI-pinned Tool opt-in. Absolute cwd, Workspace identity and State overlap use serve admission. Nonempty additional directories fail. Editor MCP servers are ignored with one fixed stderr notice per connection; no server is launched or contacted. Success commits Session creation before replying and counts toward serve's loaded-thread limit. |
| `session/load` | `{sessionId,cwd,mcpServers,additionalDirectories?}` → `{modes,configOptions}` | Maps to `thread/resume` with the same CLI-pinned execution settings as new, including Provider selection, collaboration policy, child limit and Tool opt-in. Revalidates the stored Session's Workspace identity, State separation and current roster/Provider admission. Replays bounded history as updates before replying. Unknown Sessions refuse; foreign Workspaces fail `workspaceMismatch`; active/locked Sessions fail `busy`/`sessionBusy`. |
| `session/resume` | `{sessionId,cwd,mcpServers?,additionalDirectories?}` → `{modes,configOptions}` | Same admission and history-size refusal as load, without history updates. A successful load or resume makes the Session available for subsequent prompts. Returns the same synchronized mode state as new/load. |
| `session/set_mode` | `{sessionId,modeId}` → `{}` | Requires a loaded Session; maps to serve `thread/settings/update`. Unsupported IDs fail invalid-params. Success queues both mode/config updates before replying. |
| `session/set_config_option` | `{sessionId,configId:"mode",value,type?}` → `{configOptions}` | Same mode admission and settings update as set_mode. Returns the complete selector list. Only a string value from the advertised modes is accepted; boolean values/types and other config IDs refuse. |
| `session/close` | `{sessionId}` → `{}` | Requires a loaded Session; maps to serve `thread/unsubscribe`. Treats active work as cancelled, withdraws its permission and interrupts the turn, then sends serve `thread/unsubscribe` after prompt resolution and any reconciliation, independently of output backpressure, so tap-loss reconciliation recovers a missed Uncertain observation first; the close response follows the prompt response. The Session is closing until the unsubscribe response: new prompts and closes fail `busy`. Close has a 3-second deadline (the shutdown grace) on an independent timer, so output backpressure neither delays it nor the unsubscribe, and a late completion for an expired prompt is ignored; when it expires the turn stays cancelled, the prompt replies `cancelled` with `arany.incompleteUpdates` (or `effectUncertain` when one was observed), nothing is retried and the unsubscribe is sent. Replies after committed completion and freed resources. The prompt replies `cancelled`, preserving any observed uncertain-effect count even if teardown fails. Closed history stays discoverable and can be loaded again. Unknown/unloaded IDs fail resource_not_found. |
| `session/list` | `{cwd?,cursor?}` → `{sessions:[{sessionId,cwd,title,updatedAt}],nextCursor}` | Maps to serve `thread/list` with its default page size, Workspace admission, metadata-only projection, opaque cursor checks and ordering; a page may hold fewer entries than the page size when serve's replay budget is reached, and `nextCursor` continues it. List consumes no loaded slot and never executes. Unknown params and `_meta` are ignored; no incoming metadata is reflected. |
| `session/prompt` | `{sessionId,prompt:ContentBlock[]}` → `{stopReason}` | Requires available selection and a loaded Session. Converts bounded rich content as below. No URI is fetched, opened or turned into authority. Maps to `turn/start`, starting one ordinary Goal. One prompt per process; a concurrent prompt fails `busy`, without queueing or execution. |
| `session/cancel` | Notification `{sessionId}` | Cancels that Session's active prompt through `turn/interrupt`. Before the turn ID arrives, cancellation is latched and forwarded once. Repeated/unknown cancellations do nothing. |
| `$/cancel_request` | Notification `{requestId}` | Same cancellation when the ID names the active prompt; unknown IDs do nothing. |

Capabilities declare `loadSession:true`, image/embeddedContext true and audio false,
MCP HTTP/SSE false, and `sessionCapabilities:{resume:{},close:{},list:{}}`. Client filesystem and
terminal offers are ignored. Arany never sends `fs/*`, `terminal/*` or
`elicitation/*`, and never connects editor-supplied MCP servers. Load/resume
ignore editor MCP servers with the same once-per-connection notice as new and
refuse nonempty additional directories.

### Closure and discovery

[Serve lifecycle](./serve.md#methods) owns cancellation, slot release, page/cursor
bounds, activity ordering and metadata isolation. Omitted cwd selects the process
launch Workspace; listing never enumerates other Workspaces. Returned cwd is a
display string, never a grant. The metadata projection includes no State paths,
include paths, saved defaults, credentials or Provider configuration. Titles
are escaped with the same control/bidi neutralization as Tool titles and remain
untrusted plain text. Editors must render titles, cwd, timestamps and metadata
inertly; this adapter never interprets them as markdown or instructions.
`updatedAt` is the last committed activity in UTC ISO 8601, omitted when unknown.
Discovery does not authenticate a Provider or load any Session. A malformed,
oversized, stale or foreign-Workspace cursor returns invalidParams rather than
silently starting over. Close never resumes or retries an uncertain effect.

### Approval modes

Following [ACP v1 config options](https://agentclientprotocol.com/protocol/v1/session-config-options)
and Claude Code ACP's mode selector, new/load/resume return both `modes`
(`currentModeId`, `availableModes`) and one `configOptions` entry with `id:"mode"`,
`category:"mode"`, `type:"select"`, `currentValue` and flat `options`.
Both surfaces offer exactly `default`, `acceptEdits` and `auto`, with the same
IDs, order and labels. `--permission-mode` selects the initial value (default
`default`); reloading a Session on the same connection keeps its current value.
A new process uses its CLI starting value. These are connection settings, not
replayed journal transitions or persisted changes to private configuration.

Both setters go through serve's [settings update](./serve.md#methods).
After success, `current_mode_update` and `config_option_update` (the complete
selector list) precede the response, keeping either client UI synchronized.
Failures send no mode/config update and retain the prior value. Reloading while
that Session's mode change is awaiting acknowledgment returns `busy`, preventing
stale settings from replacing a pending change. Changes during an active prompt
use the next approval decision; a pending permission request is neither resolved
nor withdrawn, its intent stays fixed, and its eventual one-use verdict
is journaled normally.

The [Tools mode composition](./tools.md#modes-and-approval-decisions) remains
independent: a settings Ask or Deny cannot be widened by either setter. A mode
changes approval handling only, never the CLI-admitted Tool grant. In particular,
selecting `auto` cannot enable Tools without `--tools`, expand paths or commands,
change Provider selection or budgets, or enable human-only approvals.
Unlike Claude Code ACP's plan and optional bypass modes, `plan` and `dontAsk`
remain private-settings-only and bypass is always refused. The fixed selector
omits these modes to preserve the admitted authority and Arany's independent
restrictions. Mode/config IDs are case-sensitive closed values; unknown and
oversized strings fail invalid-params within the existing line bound. Select
values accept an omitted `type`, `type:"id"`, or an unknown string discriminator
with a string value, matching the pinned peer SDK's forward compatibility.

### Stored history and continuation

Serve's [Provider setup refusal](./serve.md#error-model) maps to `-32603` with
`data.reason:"providerSetupBlocked"` for `session/prompt`, `session/load` and
`session/resume`, including a Session already loaded on this connection.

Load projects only the strictly reduced committed history returned by serve's
`thread/resume`: turns in history order, each user message, its Tool items, then
its committed answer when present. User messages become `user_message_chunk`;
answers become `agent_message_chunk`. Each historical Tool produces a
`tool_call` followed by its terminal `tool_call_update`, using the live Tool
projection and the client's original cwd spelling for display locations.

An unfinished Run stays interrupted. The user update's `_meta["arany.turnId"]`
and `_meta["arany.turnStatus"]` carry the historical Run ID and serve status;
`_meta["arany.images"]` carries image metadata only, never reconstructed pixels.
Unfinished intents replay as uncertain with the same explicit unknown-outcome
text and metadata as live Tools. No observation, answer or completion is
invented. Replay executes no Provider, Tool, approval, continuation or Goal retry
and appends no Events. A later plain prompt starts a new Goal in the same Session,
with fresh Run admission, as on serve; it never resumes an interrupted Goal
implicitly. Resume attaches without replaying any messages.

Serve validates the entire resume result against its 1 MiB line bound before
loading the thread or returning history. Overflow maps to `-32603 contextTooLarge`
with no partial replay or newly loaded Session. The translator retains that one
bounded result and projects at most two updates per item through the existing
bounded output queue. Ordinary serve messages arriving during a blocked
projection use the bounded deferred queue in the limits below. Existing
per-update Tool/text truncation applies. Reloading a loaded Session consumes no extra slot.

Text blocks concatenate in order without separators. A `resource_link` becomes
`[@name](uri)`. An embedded `resource` must contain `uri` and `text`; blob resources
reject. It becomes `<context ref="ESCAPED_URI">BODY</context>`, escaping `&`, `<`,
`>` and `"` in the URI and replacing the `<` of every ASCII case-insensitive
`</context` prefix in the body with `&lt;`, regardless of what follows the prefix
(including whitespace before `>`). These wrappers remain user data, never
instructions or trust.
PNG images require `mimeType:"image/png"` and base64 `data`; an image `uri` is
ignored. Image-only prompts are accepted. Conversion reuses serve's text,
base64, count and aggregate-byte admission; serve's existing image path checks
PNG structure and bounds under the [terminal image contract](./terminal.md#paste-and-attachments).
Other content types and overflow reject the whole prompt with invalid-params.

Budget-stop prompt results retain their existing `max_tokens` or `max_turn_requests` reason and add `_meta["arany.budgetRefusal"]` (the committed canonical record, or null when unavailable) and `_meta["arany.status"]` (the shared bounded Arany status). Loaded history carries the same fields on its existing user-message update; no assistant message is fabricated. These follow the [Goal refusal contract](./goals.md#budget-refusal-feedback) and preserve unknown-effect/cancellation precedence.

### Tool updates and permissions

Permission verdicts are fields of `ToolStarted`, and `ToolPermissionDecided` is absent. A permission request refers to a committed `ToolProposed` intent and timestamp; it may precede the Tool's started update. Proposals that never start create no historical Tool item. ACP's negotiated protocol version remains 1.

Only committed serve items produce Tool updates. `item/started` becomes
`tool_call` with the immutable intent ID, compiled title plus bounded escaped
intent fields, kind, pending status and locations. Command calls are `execute`,
file changes `edit` (delete-only sets are `delete`), Read/Skill/typed Git reads
`read`, List/Search `search`, GhPrList/GhPrView `read`, and MCP/other calls `other`.
GhPrList and GhPrView titles identify GitHub pull-request listing or inspection
and include bounded, escaped repository detail; their available journaled fields
remain in `rawInput`, and neither has a Workspace location. Move retains the serve
FileChange projection: `edit`, the `Change files` title with its escaped source
detail, and both source and destination locations. Available journaled call
fields form `rawInput`; file-change items expose paths without invented content
or diff bytes. Locations join the client's original cwd spelling with
Workspace-relative paths for display only; they are never re-resolved or used
for access. The typed Tool fallback matches every ToolCall variant explicitly;
only the outer non-Tool item fallback remains open.

`item/completed` becomes `tool_call_update`: success is `completed`; declined
and failed dispositions are `failed`, with the disposition named in fenced
content. An uncertain outcome is `failed` with `_meta["arany.status"]:"uncertain"`
and the text: "Outcome unknown: Arany could not confirm whether this effect
happened. It was not retried; inspect the Workspace before continuing." A first
observed terminal item is a self-contained `tool_call` with its final status;
reconciliation never replaces a terminal status with pending. Tool output is
escaped and placed inside a code fence longer than every backtick run in it.
There are no client terminals and no diff contents.

A recoverable primary denial or known Tool failure completes its Tool update as
failed and reaches the requesting model once as the correlated result. The
prompt may continue under ordinary Goal admission; its final stop reason comes
from the committed Engine outcome. Hook and permission-rule prose is excluded
from the result text.

A serve approval request becomes `session/request_permission` with the Session
ID, a self-contained pending `toolCall` update, its exact committed EffectIntent
in `rawInput`, escaped and fenced approval lines, and exactly these ordered options:

| optionId | kind | name |
|---|---|---|
| `deny` | `reject_once` | Deny |
| `allow_once` | `allow_once` | Allow once |

A selected `allow_once` maps to serve `accept`; `deny` maps to `decline`; an unknown option,
malformed result or client error records unavailable approval. A cancelled outcome maps to
`cancel` (interrupt the turn and withdraw the pending operation). The reply can resolve only its one
outstanding, already sent live request; unknown, duplicate,
withdrawn and cancelled IDs change nothing. ACP request IDs are independent of both
client request IDs and internal serve IDs. `allow_always` is never offered. The offered rejection ID is `deny`; `reject_once` is its option kind, not an accepted ID. Any other ID records unavailable approval.
Ordinary client decisions are user-configured verdicts; ACP cannot attest a
human gesture, so human-only requests follow the
[Tools contract](./tools.md#modes-and-approval-decisions). Eligible host-prefix reuse follows the same Engine authority path and emits committed unguarded Tool updates and Goal evidence; neither `allow_once` nor `allow_always` creates host authority or a remembered rule. All normal Policy,
one-use grant, journal and Guard checks still apply.

Cancellation, Tool completion or serve withdrawal retire the pending
permission and queue `$/cancel_request` for its ACP ID if it was sent. Serve
owns withdrawal independently of outbound capacity. A primary permission has no
wall-clock expiry once enqueued; a still-connected slow client remains
answerable. A client may receive an already withdrawn request or delayed withdrawal, and cannot
authorize it. Child-request withdrawal follows the [Tools approval rules](./tools.md#folder-trust-and-approvals); `$/cancel_request` follows the committed withdrawal. An oversized
permission or a full/closed ACP output queue records unavailable approval
without sending or truncating its intent
or approval lines. Enqueue is attempted once, with no retry, and its ID is
retired before reporting unavailable to serve; later replies are ignored.
Deferred-queue bypass follows [ACP limits](#limits-and-compatibility). No
transport configuration widens the private grant.

A committed serve `item/completed` agentMessage produces exactly one
`session/update` with `update.sessionUpdate:"agent_message_chunk"` and
`content:{type:"text",text}`. User input is not echoed. Provider previews,
commentary and uncommitted display text are absent. The prompt response follows
all its queued updates. Text passes through as model markdown; the editor owns
inert rendering. This transport makes no claim about editor auto-fetching.

### Goal plans

With Tools enabled, the adapter subscribes to serve's bounded committed Event
pages after turn acceptance. A `GoalPlanContractRecorded` for the active Run
produces a full replacement `plan`: two entries, the mission followed by the
success criterion, each with priority `medium`. The mission starts `in_progress`
and the criterion `pending`. Only committed completion of the Goal marks both
`completed`; failure or interruption returns both to `pending`, since ACP has
no failed plan-entry status. No plan or step is invented when the Goal has no
contract. Plans describe the current prompt's Goal; load/resume retain their
message/Tool history projection and do not resume an old Goal or its plan.

Plan content is untrusted plain text. Control/bidi characters are escaped with
the Tool display helper; paths and markup are never resolved, executed or
interpreted as instructions. Editors must render entries inertly. Exact-content
criteria name the file without disclosing the expected bytes; Tool criteria
show the bounded journaled call as data. Plans never change grants or budgets.
Duplicate/older contract Events and Events for another Session or Run are
ignored. Only one current plan is retained per active prompt. Its two content
strings are each capped at 2,048 UTF-8 bytes after escaping, plus the existing
Session-history truncation marker; the ordinary outbound line check still applies.

On a tap gap, a completed turn missing its answer, or completion with Tools
enabled, the translator reconciles committed `MessageCommitted`, `ToolStarted`,
`ToolFinished` and Goal plan facts through
serve `events/subscribe`, paging from the last reconciled Session sequence and filtering by the active Run.
Already sent answers, Tool phases and plan revisions are suppressed; the final
plan replacement precedes the prompt response. Event paging preserves the
loaded thread's pinned settings, whereas `thread/resume` would re-admit and replace them.
A replay refusal produces one fixed diagnostic and
`result._meta["arany.incompleteUpdates"]:true`. During EOF/stop shutdown no new
replay work is started; missing updates carry the same marker. If serve closes
before a recovery reply, the committed completion still receives a prompt
response with this marker. A new process never resumes or retries a Goal implicitly.

The prompt result maps serve's closed `goal.stopped` projection:

| Committed outcome | Prompt result |
|---|---|
| Run finished, Goal completed and no `goal.stopped` | `stopReason:"end_turn"` |
| Client cancellation or process stop, or `goal.stopped` `paused` or `cancelled` | `stopReason:"cancelled"`, including a teardown error after cancellation; `_meta["arany.uncertainEffects"]` reports observed uncertain Tools when nonzero |
| Failed Run (`goal.stopped` `failed`, `notAdmitted` or absent), uncertain effect (an observed uncertain Tool or `goal.stopped` `uncertain`), interruption without a terminal receipt (`interrupted`) or hook stop (`stoppedByHook`) | `-32603` with compiled recovery guidance and `data.reason:"turnFailed"`, `"effectUncertain"`, `"turnInterrupted"` or `"stoppedByHook"` |
| Budget stop (`goal.stopped` `goalExhausted`) | `limit:"tokens"` maps to `max_tokens`; `calls`, `runs` and `activeTime` map to `max_turn_requests`. Cancellation keeps `cancelled` and observed uncertain effects keep the failure path. An admission failure without a committed turn stays an RPC error. |

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
| Serve outbound / ACP outbound | 256 messages each, plus at most 256 deferred serve messages while a projection waits. Deferred-message overflow fails the connection with Session-history recovery guidance. Permission requests bypass that deferred queue and immediately resolve unavailable if ACP output is full or closed. Server permission withdrawals also bypass the queue: the old reply ID is retired immediately, with at most one cancellation notification waiting for output capacity before a new permission can be delivered. Projection waits for ACP capacity independently of input/cancel. If immediate replies fill ACP output, input fails closed and starts shutdown rather than holding cancellation behind stdout. |
| Pending translations | 64, with 61 available to client requests and three reserved for control, plan subscription and reconciliation; excess requests fail `busy`. Integer internal IDs are checked and never reused. |
| Sessions / active prompt | Serve's 64 / 1, with `threadLimit` / `busy`, no eviction. Close frees a slot after cancellation settles. |
| Prompt blocks / aggregate text after rich-content conversion | 64 / 64 KiB UTF-8, including mention/context syntax; excess or empty/whitespace-only text without images fails `invalidParams`. |
| Images | [Serve](./serve.md#limits-and-compatibility) and [terminal](./terminal.md#paste-and-attachments) image limits; reject overflow without truncation. |
| Session list | Serve default page size and cursor/line bounds; no client page-size option is advertised. |
| Mode selectors | One config selector and exactly three mode values; no client-supplied selector list is retained. Setters share the 61 client-request slots, 64 Sessions and line cap; invalid values refuse without changing state. |
| Tool content / title | 64 KiB / 256 bytes of escaped text plus a Session-history truncation marker. Fences are longer than every contained backtick run. If the full line still exceeds the cap, optional raw input/output and locations are omitted and content becomes a history marker. |
| Goal plan | Two entries and one retained revision per active prompt; each entry has 2,048 UTF-8 bytes of escaped content plus a history marker; overflow truncates at a UTF-8 boundary. Event paging and live forwarding reuse serve bounds. |
| Pending permissions | One; no primary wall-clock expiry. Oversize or full/closed output resolves unavailable without sending. |
| Oversized answer projection | Cut at a UTF-8 boundary to at most 128 KiB text plus `[truncated; see arany show SESSION_ID]`; worst-case JSON escaping still fits the line cap. Other oversized projections fail closed. |
| Reconciliation | Serve's bounded Event pages and Session Event cap; no retained history beyond the current page, one active answer, at most 10,000 deduplication entries for Tool phases per prompt, and one cursor plus original cwd per Session; deduplication overflow suppresses further Tool projections and marks incompleteUpdates. |
| Load/resume history | [Stored history and continuation](#stored-history-and-continuation) and [serve's Session Event cap](./serve.md#limits-and-compatibility). |
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
| Optional surfaces | Every closed `goal.stopped` value, including the action/token budget limits, maps exactly, without treating ordinary failure at a budget boundary as exhaustion; bounded mission/criterion plans use committed facts, neutralize controls, suppress duplicate/foreign Events and precede the response; unadvertised controls refuse | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Approval modes | New/load/resume selectors and both update notifications agree; either setter changes both surfaces; unsupported/oversized modes, boolean config and unknown Sessions refuse; mid-turn switch retains a pending permission, changes the next approval, and cannot widen settings Ask | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs), including its native Guard journey for automatic edits |
| Session lifecycle | Close frees capacity after 64, keeps durable history and cancels a turn before replying; uncertain outcomes retain unknown metadata without retry; list paginates within bounds, refuses invalid/foreign cursors and exposes only Workspace metadata with escaped titles | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Cancellation and capacity | Busy refusal runs no second prompt; session and request cancellation, pre-accept latch, EOF and stalled output produce bounded interruption without invented success | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Hostile input and recovery | Oversized/malformed lines recover; resource links remain literal mentions within block/UTF-8 bounds; relative cwd, additional directories, unsupported/oversized prompts and ambiguous envelopes refuse; offered fs/terminal and editor MCP never execute; EOF during recovery preserves the committed completion with incompleteUpdates | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Shipped command | TTY refusal, including with `--tools`, exits 2 before State; cleared credentials initialize successfully, auth/new fail auth_required, no Session, EOF exits 0 with empty stderr and schema-valid stdout | [Session process corpus](../../crates/arany-cli/tests/session_run.rs) |
| Editor launch directory | A launch directory containing State still permits initialize, profile authentication and Session creation for a different cwd; a session cwd containing State refuses without creating a Session | [Session process corpus](../../crates/arany-cli/tests/session_run.rs) |
| Provider setup blocks continuation | Pending and Uncertain setup edits refuse prompt, load and resume with `-32603 providerSetupBlocked`; Events remain unchanged and inference/compaction Provider calls stay at zero | [`setup_edits_block_loaded_session_prompts_without_events_or_provider_calls`](../../crates/arany-cli/tests/acp/in_process.rs) |
| Stored Session load/resume | A second server over the same State replays committed user/answer and Tool history before its schema-valid response; resume sends no history; neither executes nor changes Events; a later prompt appends a fresh Run to the same Session | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Pinned collaboration admission | New, load and resume retain CLI `single` or a zero child limit; a later prompt requesting delegation spawns no child in committed history | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Load admission and recovery | Unknown Sessions, foreign Workspace identities, relative/State-overlapping cwd, additional roots, active/locked Sessions and oversized history refuse without partial replay; unfinished Runs/intents retain interrupted/uncertain facts without execution | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Admitted Tool permission round trip | Allow dispatches once; reject denies; reopened intent, verdict and observation equal the permission payload and committed projections | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Permission correlation and withdrawal | Only allow_once/reject_once offered; a late reply to a live primary request remains answerable; unknown, duplicate, withdrawn or invalid/error replies cannot authorize a Tool; client cancellation and server withdrawal remain effective under output backpressure | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Rich prompt and inert Tool data | Oversized resources, blobs, image MIME/base64/count/byte overflow reject; context closing tags neutralized; PNG and image-only conversion, original cwd spelling, uncertain metadata, escaped titles, longer fences and truncation markers validate against the pinned schema | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Host-prefix H1 policy | Existing eligible rules execute only in default/dontAsk without a permission request; auto/acceptEdits, explicit ask/deny and absent rules dispatch nothing; forged client approvals add no authority; closed replay retains the host receipt | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Human-only Asks unavailable | Hook Ask under Auto and unguarded proposals, even with exact project rules, make no client request, dispatch nothing and replay unavailable_approval | [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs), [Guard permission corpus](../../crates/arany-cli/tests/guard_runs/permissions.rs), [unguarded corpus](../../crates/arany-cli/tests/guard_runs/unguarded.rs) |

The ACP corpus also owns `guarded_acp_permissions_publish_once_and_refuse_hook_asks`,
a CI native gate using Cargo's built Guard executable. It checks one allowed edit,
Workspace bytes and the reopened Guard receipt through the full adapter, plus a
hook Ask that sends no permission request and replays `unavailable_approval`.
Local private-State and native restrictions do not count as passing this gate.

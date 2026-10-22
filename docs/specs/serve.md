# Serve protocol and schema

## Scope and related contracts

This contract owns `arany serve --stdio`: its JSON-RPC vocabulary, bounded
transport, thread and turn projection, and JSON Schema export. The server runs
`initialize` / `initialized`, `thread/start|resume|fork|unsubscribe|list`, `turn/start|interrupt|steer`,
`thread/settings/update`, `thread/rewind/preview`, `thread/rewind`, `goal/pause|resume`, `events/subscribe` and `receipt/get`.
The server sends one-use human approval requests. The remaining work belongs to
[NEXT_STEPS item 8](../../NEXT_STEPS.md#pursue-in-order).

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
responses can carry a null ID. Server request IDs reserve a separate integer
namespace starting at 1.

Requests carry `id`, `method` and optional `params`; notifications omit `id`.
Responses carry `id` and `result`; error responses carry `id` and
`error: {code, message, data?}`. `data` contains a closed camelCase `reason`.
Envelope fields and typed params/results reject unknown fields, except
`initialize.capabilities`, whose unknown keys are ignored for feature detection.
Unknown notifications are ignored. Client responses and error responses that match no
pending approval request (unknown, late, duplicate or null-ID) are dropped silently:
server and client request IDs are separate namespaces, so an answer could be mistaken
for a reply to the client's own request.

Protocol fields use camelCase. Embedded journal vocabulary (`EventLine`, Event
payloads, Tool calls and intents) retains its existing snake_case bytes; receipt
budgets, usage and reports use camelCase.
`EventLine.payload` is an open object: exporting a schema does not redefine the
journal format. JSON is data; clients must render untrusted content inertly.

### Error model

| Code | Meaning | `data.reason` |
|---|---|---|
| `-32700` | Malformed message; null ID | Absent |
| `-32600` | Invalid protocol state or oversized message | `notInitialized`, `alreadyInitialized`, `messageTooLarge`, `busy`, `threadNotLoaded`, `threadLimit`, `turnNotActive`, `serverShuttingDown` |
| `-32601` | Unknown method | Absent |
| `-32602` | Invalid parameters or unsupported permission mode | `invalidParams`, `unsupportedPermissionMode`; steering refusals `runFull`, `empty`, `tooLarge`, `unavailable` |
| `-32603` | Internal channel or serialization failure | Absent |
| `-32000` | Engine admission failure | EngineError variant in camelCase; the closed vocabulary is exported as `$defs.Reason` |
| `-32001` | Provider selection or routing failure | `providerUnavailable`, `providerRejected`, `invalidSelection` |

Engine errors use their compiled Display message. A failed accepted turn is
represented by `turn/completed` with status `failed` and compiled recovery
guidance in `error.message`, never a JSON-RPC error. Cancellation is
`interrupted`. An unknown Tool effect remains `uncertain`, never successful or
merely failed, preserving the honest-uncertainty guarantee. If an Engine error
leaves an accepted submission without a committed terminal Run, the process
exits unsuccessfully with Session-history recovery guidance rather than
inventing a completion.

## Methods

| CLI entry | Observable behavior |
|---|---|
| `arany serve --stdio [--state-dir DIR]` | Refuses terminal stdin or stdout before State admission, exit 2 with `error: serve requires piped stdin and stdout`. Omitting `--stdio` exits 2 with `error: unsupported transport; pass --stdio`. Clean EOF cancels any active submission, drains its committed notifications, closes the Engine and exits 0. Input failure performs the same shutdown and exits 1. Transport, storage or shutdown failure exits 1 with `error: …` on stderr. |
| `arany generate-schema` | Writes one pretty-printed JSON Schema document plus a final newline to stdout; stderr is empty on success; no State, credentials or Engine are opened. |
| `arany generate-schema --out DIR` | Creates DIR as needed and atomically replaces `DIR/arany-protocol.schema.json` using an exclusively created temporary file in DIR; success leaves stdout and stderr empty. Refuses an existing symlink or non-file target. Failure exits 1 with a compiled `error: …` diagnostic on stderr. Success exits 0. |

The document is titled `arany-protocol`, carries schema artifact `version: "1"`,
uses JSON Schema draft 2020-12 and sorted object keys, and contains `$defs`,
`methods`, `notifications` and `serverRequests`. Registry entries reference
schemas in `$defs`; the document root validates JSON-RPC envelopes. Consumers
validate method-specific params/results through the corresponding registry
entry with the same `$defs`. Output is deterministic for a fixed contract.

| Method | Params and result | Ordering and admission |
|---|---|---|
| `initialize` | `{clientInfo:{name,title?,version},capabilities?:{optOutNotificationMethods?:[string]}}` → `{userAgent,platformOs,capabilities}` | First request; other requests before it return `notInitialized`; a second initialize returns `alreadyInitialized`. Returns no paths or credentials. `initialized {}` is the following client notification and has no response. |
| `thread/start` | `{cwd,provider,model,effort?,title?,collaboration?,maxActiveChildren?,tools?,permissionMode?,includePaths?}` → `{thread}` | Selects the Provider and configured role chains as `exec` does. `Engine::create_session` commits `SessionStarted` before the result and subsequent `thread/started`. A thread is a durable Session, including when no turn follows. |
| `thread/resume` | `{threadId,cwd,provider,model,effort?,collaboration?,maxActiveChildren?,tools?,permissionMode?,includePaths?}` → `{thread,turns}` | Admits the selected Workspace and execution configuration with the same settings and defaults as start, strictly replays history without executing it, and loads the thread. Turns contain historical user, Tool and answer items; an unfinished Run and Goal appear interrupted. |
| `thread/unsubscribe` | `{threadId}` → `{status:"unsubscribed"\|"notLoaded"}` | Codex app-server shape. Removes the connection's loaded thread and raw subscription; durable Session history remains. An active submission has its approval denied and cancellation signalled, and its committed completion precedes the response and slot release. The thread stays loaded until then so `events/subscribe` reconciliation of the completed turn still works; ACP close ordering is defined by [session/close](./acp.md#behavior). A second close while cancellation is pending returns `busy`. An unloaded ID returns `notLoaded` without inspecting State. |
| `thread/list` | `{cwd?,cursor?,limit?}` → `{data:[{id,title,cwd,createdAt,updatedAt}],nextCursor}` | Read-only Session metadata for one admitted Workspace, including unloaded/closed Sessions. Absolute cwd defaults to the process launch directory. Newest committed activity first. Each page defaults to 50 entries, with limits 1–100; `nextCursor:null` ends the list. Invalid, oversized, noncanonical, stale-anchor or foreign-Workspace cursors fail `-32602 invalidParams`. No Provider selection, Run or Tool execution, and no loaded slot is consumed. |
| `thread/settings/update` | `{threadId,permissionMode}` → `{permissionMode}` | Requires a loaded thread (`threadNotLoaded` otherwise). Replaces its live approval mode and updates the matching active submission's `ToolApprovals`, like terminal Shift+Tab. A pending immutable approval keeps its intent, deadline and decision channel; only subsequent approval decisions use the new mode. Other loaded threads are unaffected. No Provider selection, Tool grant, settings snapshot, Goal budget or child authority changes. |
| `thread/fork` | `{threadId,cwd,title?}` → `{thread}` | Requires a loaded source and capacity for another thread. Calls `Engine::fork_session`: the Workspace identity must match, the source must be unlocked and end at a committed terminal Run boundary. Inherits the source's pinned execution configuration. The result precedes `thread/started` with `forkedFromId`; empty prefixes, an interrupted objective before `RunStarted` and an open Run fail `forkNotAtRunBoundary`, a busy source fails `sessionBusy`. A reply that cannot fit a line (client-sized request ID) fails `contextTooLarge` before any fork is persisted. |
| `events/subscribe` | `{threadId,since:u64}` → `{events:[EventLine],nextSince:u64\|null}` | Requires a loaded thread. Strictly replays committed Events with `sequence > since`; `since` beyond that thread's head fails `invalidParams`. Follow each non-null `nextSince` to drain the backlog. The final page enables live forwarding after its result; repeating a subscription replaces its cursor. |
| `receipt/get` | `{threadId,goalId}` → `{goal:{id,status,objective,budget,usage,report?}}` | Requires a loaded thread and a Goal in its strictly reduced history; otherwise `threadNotLoaded` or `goalUnavailable`. Returns only journaled receipt facts, with no State paths, Workspace metadata, credentials or runtime configuration. `report` exists only after committed `GoalFinished`. An oversized result fails `contextTooLarge`, without partial receipt data. |
| `turn/start` | `{threadId,input:[{type:"text",text}\|{type:"image",mediaType:"image/png",data:base64}],goal?:{autonomous}}` → `{turn}` | Requires a loaded thread. Uses its pinned configuration and the existing Engine submission path. The result follows tapped `MessageAccepted`, before `turn/started` and user item notifications. If that tap is lost, successful finalization answers from the first committed Run accepted after the submission began. The turn ID is the committed Run ID, with a separate Goal ID. One submission may be active per process; another returns `busy`. |
| `turn/interrupt` | `{threadId,turnId}` → `{}` | Signals the matching active Run's cancellation. Unknown or finished turns return `turnNotActive`. The empty result acknowledges the signal; `turn/completed` reports interruption only after committed `RunFinished`. |
| `turn/steer` | `{threadId,turnId,input:[{type:"text",text}]}` → `{turnId}` | Offers one note to the matching active Run's `RunSteering`, the call the attached composer makes. Exactly one text of at most 64 KiB, no images. Unknown or finished turns return `turnNotActive`; between autonomous Runs the last Run's ID still matches and the note waits for the next Run's first primary call, as in the terminal; an `Empty`, `TooLarge` (over 8 KiB with its history label), `RunFull` (the Run's 8 KiB note budget) or `Unavailable` refusal is `-32602` with that reason. Success means the note waits for the Run's next primary reservation (merged with any waiting note); delivery is the committed `GoalSteeringNoteRecorded`, and a note still waiting when the Run ends is discarded. |
| `goal/pause` | `{threadId,goalId}` → `{}` | Sets the sticky pause on the active Run's steering handle; the Engine stops at its next boundary as for the terminal's `/goal pause`, so the turn ends `interrupted` and the Goal stays resumable. Repeating it is idempotent, and it is accepted for the whole submission, including between autonomous Runs. No active turn on the thread is `turnNotActive`; a `goalId` other than the active turn's Goal is `-32000 goalUnavailable`. An in-flight Provider call is never aborted: its answer still commits and the turn can complete. |
| `goal/resume` | `{threadId,goalId,input?:[{type:"text",text}]}` → `{turn}` | Starts a submission like `turn/start` (same `busy`, `threadNotLoaded`, admission, approval and steering behavior) with `GoalRequest::resume(goalId, true)`, exactly as the terminal's `/goal resume` does. Only the thread's most recent resumable Interrupted Goal (paused or interrupted) resumes; any other Goal is `-32000 goalUnavailable` before a Run. Absent input runs the pinned objective. Text is a superset of the terminal (as in `exec --resume-goal ID PROMPT`): when the Goal has no accepted Run yet, text different from the pinned objective is `goalUnavailable`; otherwise it becomes the resumed Run's user message while the Goal objective stays pinned. No images. Budget, phase and resource settings stay pinned. An exhausted Interrupted Goal closes Failed and returns `-32000 goalBudgetExhausted` with no Run, `GoalResumed` or `turn/started`. |

Lifecycle method names, unsubscribe status and list pagination envelopes follow
[Codex app-server](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/thread.rs).
One connection owns all loaded threads, so `notSubscribed` cannot occur. Arany
cancels and releases immediately after durable completion rather than keeping
an idle thread alive on a peer grace timer. Discovery deliberately narrows the
peer's global listing to one Workspace to avoid disclosing other Workspace or
State paths. The list row is a metadata-only subset: timestamps are UTC ISO 8601
strings (or `Unknown` when Store cannot represent the timestamp), rather than
Codex's epoch seconds, to reuse the Session metadata owner without replaying
full histories. No stored defaults, include paths, provider configuration,
State paths or journal content are returned. Titles are untrusted plain text;
JSON escaping does not make them instructions or trusted markup. Clients must
render every metadata field inertly.

Cursors are opaque, at most 128 UTF-8 bytes, and bind a Session ID and its last
committed sequence to the selected Workspace. They confer no access authority.
If the anchor changes, restart discovery without a cursor; new activity before
the anchor is visible on a fresh first page. A page is not a retained snapshot.
Listing resolves Workspace membership from each Session's first Event only, then replays just the requested Workspace's returned Sessions (with their fork sources) newest first. A page stops early, with a cursor, once the replayed Events reach the [Session discovery bound](./execution.md#persistence-bounds) (the first Session is always returned), so pages may hold fewer than `limit` entries; `nextCursor:null` alone ends the list. A listing never holds a read lock across decode or replay: it reads one global Event watermark, then every statement is an index seek or range (`events_by_session`, at most 256 raw rows) at or below it, with no table scan or SQLite sort (the per-Session Event count is an index range bounded by the [Session Event cap](./execution.md#persistence-bounds)): heads are walked one `session_id > ?` seek at a time and fork lineages are merged per Session lane in Rust. Each statement waits at most 2 seconds for a committing writer's lock on a read connection; writers keep 250 ms. On `SQLITE_BUSY`, watermark capture, head discovery and each page read independently allow at most three retries after releasing all statements and discarding incomplete results. Discovery and page retries retain the captured watermark and the same Event and memory bounds; each phase performs at most four attempts, with no retries of appends or other effects. Exhaustion fails the listing with the retryable `store` error "history is busy with a committing write; retry the listing", never a partial page. Each statement finishes before its rows are decoded, so Engine appends are never blocked, Sessions and their fork sources are read at the same watermark, and returned `updatedAt`/cursors match it. Raw bytes in memory are bounded by one 256-row chunk per open lineage lane, and total work by the Event budget. Head discovery costs index seeks plus one bounded count per Session head per page, bounded by the [discovery head limit](./execution.md#persistence-bounds) (`tooManySessions` beyond). Other Workspaces' history is never replayed.
A result that cannot fit the line cap fails `contextTooLarge` without
partial output. Unsupported sorting, filtering and archival options refuse as
unknown parameters.

Providers are `openai`, `anthropic`, `chatgpt` or `custom:NAME`; credentials come
from the existing selection path. Collaboration defaults to `auto` with three
children and accepts `single`, `auto` and `team` with the Engine's existing
limits. Permission modes use Claude Code spellings `default`, `acceptEdits`
and `auto`; `plan`, `dontAsk` and bypass spellings are rejected on this wire surface. Tools default
to disabled; enabled Tools use the same private configured grant as `exec`.
Absolute `includePaths` must lie under `cwd` and are translated to
Workspace-relative include paths before Engine admission. Goal and resource
settings come from the existing private settings owner and are pinned per
loaded thread. Plain input starts a new Goal; `goal.autonomous:true` uses the
Engine's existing autonomous lifecycle. `resumeGoalId` is reserved and rejected;
use `goal/resume`. Steering and pause add no notification: their effects appear as
the following committed Events and turn completion.

Receipt `usage` contains `runs`, `providerCalls`, `activeMs`, `uncertainActiveMs`,
`knownTokens`, `knownCachedTokens` and `tokenUsageUnknown` from strict replay;
it adds no elapsed wall time or inferred Provider usage. `budget` is the pinned
Goal budget. `report` is the exact committed Goal report, including its cache
counter and uncertainty facts. Objective and report text remain journal data.
Unknown threads on all three methods return `threadNotLoaded`.

## Rewind

`thread_rewind_v1` advertises two idle, loaded-thread methods. Their vocabulary follows Codex app-server's thread/turn shape and Claude Agent SDK's explicit file-rewind embedding precedent. They call the same Engine as the [terminal](./terminal.md#rewind); [Tools](./tools.md#checkpoints-and-rewind) owns guarded publication, drift states, limits and uncertainty, and [Session](./session.md#conversation-rewind) owns fork semantics. No Tool approval mode, model response or repository text confirms a rewind.

| Method | Params | Result |
|---|---|---|
| `thread/rewind/preview` | `{threadId,turnId}`; turnId identifies the selected Run, or the UUID of a Before rewind checkpoint | `{checkpoint:{kind:"run"\|"rewind",id,sequence},restore:["both"\|"conversation"\|"code"],files:[{path,state,added,removed}],blockers,nonRewindable:[{sequence,effect}],previewToken,expiresAtMs}` |
| `thread/rewind` | `{threadId,previewToken,restore:"code"\|"conversation"\|"both",kept?:[path],overwrite?:[path],confirmOverwrite?:bool}` | `{rewind:{id:string\|null,outcome,conflicts,conflictCount},thread?,conversation?:{threadId:string\|null,prompt:string\|null,imagesOmitted},notices:[string]}` |

Preview performs no publication or fork. Restore choices contain eligible modes, in terminal order; conversation stays available across unknown code outcomes or read-only access. Files use the closed camelCase states restore, delete, recreate, drifted, historyDrift, unavailable, unsafe, outsideGrant and kept. Added/removed counts use the [terminal comparison convention](./terminal.md#rewind), or null for binary/unavailable comparisons. Blockers are uncertainPublication, noWriteGrant or guardUnavailable; non-rewindable effects are host, unguardedCommand, mcp, cache or directory. The host marker covers the human-approved Git remote effects specified by [Tools](./tools.md#human-approved-host-git). Paths and text remain untrusted JSON data; clients render them inertly. Diff content is not sent on this surface.

One opaque, one-use preview token is retained per connection; any new preview replaces it, and it is bound to thread, checkpoint, exact head/prefix, Engine plan, observed file states and the [Engine confirmation expiry](./tools.md#checkpoints-and-rewind). A new submission, unsubscribe or connection close discards it. Stale, foreign-thread, replaced, reused or expired tokens return `previewExpired`; no persistent token or renewed authority survives restart. Mutation consumes the token, including a refusal after admission. Live file/grant/blob/enforcement admission is rechecked by the Engine. Kept/overwrite selections follow the [Tools rewind limits](./tools.md#checkpoints-and-rewind); overflow is `invalidParams` before effects. Re-previewing a selection must retain the original observed states, so an edit after preview returns previewExpired rather than adopting a new overwrite precondition.

The parent client is the explicit human confirmation channel. A `thread/rewind` request confirms exactly the previewed plan and named keep selection. Any overwrite selection additionally requires `confirmOverwrite:true`; omitted/false returns `overwriteConfirmationRequired` without consuming the token or writing. The client must present the listed paths, saved-current-content warning and a separate explicit overwrite decision, with Cancel initially selected, as the terminal does. This is never AI approval or automatic permission-mode approval. A restricted adapter such as ACP returns humanConfirmationRequired; it cannot inherit this capability as an automatic mutation path. Code admission follows the [Tools rewind policy](./tools.md#checkpoints-and-rewind); blocked publications return `rewindBlocked`.

Code outcomes are restored, conflict, denied, failed, cancelled or uncertain. Conflict returns the bounded conflict list and total; notices include capture/pruning and unknown-outcome guidance. Raw subscriptions carry the canonical rewind Events. Existing turn/item notifications are unchanged. Both mode restores code before forking and preserves its receipt with a `Code restored; conversation unchanged` notice on later fork failure or cancellation. Conversation-only restoration has a null rewind ID and creates no Workspace rewind Events. A durable fork appears in thread and is loaded with the source execution configuration; the response precedes thread/started. Restoring the first prompt returns conversation.threadId null and no thread: the client opens an unsaved conversation, retaining the returned draft, and uses thread/start only on explicit submission. Omitted prompt text or images are identified by null prompt/imagesOmitted. Source history and Goal budgets remain intact.

An active submission or another request during publication returns busy; an unknown loaded ID returns threadNotLoaded. An invalid checkpoint is invalidParams. Code refusals use rewindBlocked, previewExpired or rewindTooLarge; lineage overflow uses conversationRewindLimit. No method retries. Input EOF or process shutdown cancels an in-flight publication and uses the existing shutdown grace, retaining only committed outcomes. Responses and previews obey the 1 MiB line bound; a maximal receipt reservation rejects an oversized request ID before any effect or fork. The single pending Engine preview retains files and comparison versions only within the [Tool action, comparison and transfer limits](./tools.md#checkpoints-and-rewind).

## Notifications and items

All live facts come from committed Events on `RunHandle.events` or the single
process-wide `Engine::subscribe()` tap for raw subscriptions, never
Provider previews or `RunHints`. Completion reloads the strict reduced view
to include a following Goal receipt and recover a missing terminal tap. Each
submission is driven independently of the bounded outbound follower, so a slow
client cannot hold Journal appends. Notification opt-outs from initialize are
honored.

| Notification | Committed source and payload |
|---|---|
| `thread/started` | Durable Session creation; `{thread}` after the thread/start or thread/fork result. |
| `turn/started` | `MessageAccepted`; `{threadId,turn:{id,goalId,status:"inProgress",items:[]}}`. Each subsequent Run of an autonomous Goal has a new turn ID and the same Goal ID. |
| `item/started` | `MessageAccepted` or `ToolStarted`; `{threadId,turnId,item,startedAtMs}`. |
| `item/completed` | `MessageAccepted`, `MessageCommitted` or `ToolFinished`; `{threadId,turnId,item,completedAtMs}`. User items start and complete together; final answer items complete without a started notification. |
| `turn/completed` | `RunFinished`, or the end of a settled submission whose last Run has no terminal receipt, plus reduced Goal state; `{threadId,turn:{id,goalId,status,stopReason?,items:[],error?,goal:{status,stopped?}}}`. Status is `completed`, `interrupted` or `failed`. An autonomous submission can complete several turns before its final receipt. |
| `events/committed` | After a thread's final subscription page, `{threadId,event:EventLine}` for each later tapped Event in commit order. Events at or before the page head are suppressed. |
| `events/resync` | A tap `Resync {missed}`; `{threadId,missed,lastSequence}`. Raw subscriptions receive one notice per subscribed thread with its last delivered sequence and are then disabled until `events/subscribe` is called again, so a later notice never reports a cursor beyond the first gap. The turn projection also emits `events/resync` for its active thread with its own `lastSequence`, so a subscriber can see two notices; `missed` counts the process-wide tap loss and may include other threads. Recover by paging `events/subscribe` from `lastSequence`, or reconstruct items with `thread/resume`. Live delivery is best effort: a lost append acknowledgment or a fork committed inside Store can leave a gap without Resync; paging supplies completeness. |

`goal.stopped` is the closed stop that one Engine projection
(`SessionView::run_stop`) derives from committed Goal and Run facts; it equals
the submission's `RunResult.stopped`, so a live turn and its `thread/resume`
history agree. It is absent while the Run is open and when the Run finished
without stopping its Goal. Otherwise it is one of:

- `{reason:"goalExhausted",limit}`, where `limit` is `tokens`, `calls`,
  `activeTime` or `runs`, following [Goal exhaustion projection](./goals.md#lifecycle-budgets-and-recovery).
- `paused`: the Goal waits at this latest Finished or Cancelled Run for an
  explicit resume.
- `cancelled` or `stoppedByHook`.
- For a Run with no terminal receipt: `uncertain`, `goalExhausted` or
  `interrupted`, following [accepted outcomes](./execution.md#accepted-outcomes).
- `{reason:"failed",failure}` with the journal's snake_case `RunFailure`. A
  failure on the last permitted call is not budget exhaustion.

When an autonomous submission's next Run is refused after a committed Run, the
final live completion of that Run carries `{reason:"notAdmitted",refusal}` with
the closed Engine `Reason` and the compiled message in `turn.error`; no Event
records the refusal, so its history projects `paused`. A settled submission
whose last Run has no terminal receipt also completes that Run once, with
status `interrupted`; an Engine error keeps the fatal recovery path below.

`turn.stopReason` is derived from it: `tokenBudgetExhausted` for the `tokens`
limit and `actionBudgetExhausted` for the others. Intermediate autonomous turns
omit both.

Raw Event delivery and turn/item projection use separate subscriptions, so
`events/committed` may precede or follow the matching `item/*` notification.
Replaying a page neither executes effects nor repeats Provider calls. A partial
page disables live forwarding until the backlog is drained, preventing a live
cursor from skipping unreturned history. Slow clients never hold Journal appends.
An individual Event that cannot fit a live line emits `events/resync` with
`missed:1` and disables that subscription; replay refuses it with `contextTooLarge`.

Raw subscription pages and `events/committed` forward canonical journal Events of every kind unchanged to the attached local client. This includes the canonical, privacy-reduced `SupervisorVerdictRecorded` payload with its `features` and `evidence`: counts, sequences and closed enums, never paths or content. Unlike the further-reduced fixed vocabulary used by the [terminal](./terminal.md#conversation-progress-and-history) and [OTLP](./telemetry.md#spans), serve retains those canonical fields. `generate-schema` includes its kind in the closed Event vocabulary. Its payload remains the journal's open object on this wire surface, with the [execution format and bounds](./execution.md#persistence-bounds) enforced by strict replay. It creates no item or approval request. Existing pagination, line-size refusal and tap-loss/resync behavior apply; observers neither recompute verdicts nor change Run control.

Times are committed Event timestamps, never a presentation clock. Item IDs are
`<runId>:user`, `<runId>:answer`, or the immutable Tool intent UUID.

| Item tag | Fields and projection |
|---|---|
| `userMessage` | `id,text,images:[{index,mediaType,bytes}]`; metadata only, no pixels. |
| `agentMessage` | `id,text,phase:"final_answer"`; committed final answer only. |
| `commandExecution` | `id,command,args,cwd,status,disposition?,output?,attestation?`; program/argv from the intent, attestation `guard` or `inProcess`. |
| `fileChange` | `id,changes:[{path,kind}],status,disposition?,output?`; Write/Edit/Mkdir/ApplyChangeset; kinds `add`, `update`, `delete`, `mkdir`; no diff bytes. |
| `mcpToolCall` | `id,server,tool?,arguments?,status,disposition?,output?`; MCP call or catalog listing. |
| `toolCall` | `id,call,status,disposition?,output?`; remaining typed Tool calls retain their journal JSON. |

Tool item status maps `succeeded` to `completed`, `denied` to `declined`,
`failed|conflict|limit|cancelled` to `failed`, and `uncertain` to `uncertain`.
A Tool with `cache_update:uncertain` is also uncertain even if its disposition
is succeeded. Unfinished historical intents are uncertain with no invented
observation or disposition. Output is the bounded journaled observation.
The additional uncertain status diverges from Codex to preserve an unknown
effect outcome.

## Server requests

The server advertises `approvals_v1` and sends requests with a fresh integer ID
starting at 1 for each connection, independent of client request IDs. IDs are
never reused across turns. Requests go only to the client owning the turn.

| Method | Tool family |
|---|---|
| `item/commandExecution/requestApproval` | Command and explicit unguarded command |
| `item/fileChange/requestApproval` | Write, Edit, Mkdir and ApplyChangeset |
| `item/tool/requestApproval` | All remaining calls, including MCP, Skills and reads |

All carry `{threadId,turnId,itemId,startedAtMs,expiresAtMs,intent,lines}`.
`intent` is the exact journaled immutable EffectIntent; `itemId` is its UUID,
`turnId` its Run, `startedAtMs` the matching committed ToolStarted timestamp,
and `expiresAtMs` its pinned expiry. `lines` uses the terminal's sanitized human
approval renderer, including its full unguarded warning when applicable.
Failure to recover the matching committed intent, or a request above the
message size limit, denies without sending a request.

The client returns `{decision:"accept"|"decline"|"cancel"}`. Accept resolves
only that one pending ToolApproval; decline denies; cancel denies then signals
turn cancellation. A matching client error or invalid result denies. Unknown,
expired, cancelled, finished or already resolved request IDs never change a
decision and get no reply. `acceptForSession` is excluded because an immutable
one-use intent cannot become a remembered permission rule.

Unanswered requests expire at the existing Engine approval deadline. EOF,
turn interruption, completion and output closure deny outstanding requests.
Approval delivery, expiry and cancellation run independently of outbound
notification progress: a full outbound queue cannot hold Journal appends or
prevent expiry. A request still waiting for outbound capacity at expiry is
never sent. A request already queued can reach a slow client after expiry;
its reply is late and cannot authorize anything. At most one approval is
pending per connection. The Engine can abandon an ask before its intent expires
(a child's delegation deadline), which clears the pending request at once; a
request that still arrives while another is pending denies immediately without
evicting or renewing the first. The Engine inbox additionally has one
bounded ingress slot.

This client is the human approval channel. Human-only requests, including
`command_unguarded` and hook asks, always require an explicit human client
decision; this transport adds no AI or automatic approval. Engine permission
composition, intent expiry, cancellation, Policy, one-use grants and Guard
admission still apply. Hook execution remains owned by the Tools contract.

## Limits and compatibility

| Bound | Limit and overflow |
|---|---|
| Encoded or decoded line, including newline when present | 1 MiB; reader discards overflow through its newline and emits `-32600`, null ID, `messageTooLarge`; malformed input emits `-32700`, null ID; the next valid line remains usable. Outbound serialization stops at the bound. |
| `initialize.clientInfo` strings | 256 UTF-8 bytes each; invalid params. |
| Notification opt-outs | At most 32; invalid params. |
| Turn input | At least one input item, at most one text of 64 KiB; base64-encoded images follow the [terminal image contract](./terminal.md#paste-and-attachments), including its image count and aggregate decoded-byte limit; invalid params. Text or images must be nonempty. |
| Pending inbound requests | 64; reader awaits capacity, applying pipe backpressure; reader buffer 64 KiB. |
| Outbound queue | 256; follower waits and Journal lag is reported through `events/resync`. |
| Loaded threads / active submissions / pending approval requests | 64 / 1 / 1; `threadLimit` / `busy` / deny excess approvals; loaded threads are never evicted. Reloading a known thread consumes no extra slot; unsubscribe frees one after cancellation settles. |
| Session list page / cursor | 1–100 entries (default 50) / 128 UTF-8 bytes; invalid limits/cursors refuse with invalidParams. The whole response must fit 1 MiB. |
| Resume response | Must fit the outbound line bound; otherwise `-32000 contextTooLarge`, without partial history. |
| Steering and resume input | One text of at most 64 KiB, bounded further by the 8 KiB per-Run note budget for `turn/steer`; invalid params otherwise. |
| Event replay backlog / page | Strict Store replay is bounded by the [Session Event cap](./execution.md#persistence-bounds), including inherited history; exceeding that existing bound fails closed with `store`. Each response contains at most 1,000 Events and fits 1 MiB including envelope, request ID and newline; byte overflow shortens the page and count overflow continues via `nextSince`. An Event that cannot fit alone fails `contextTooLarge`. No partial oversized response is sent. |
| Raw live subscriptions / tap backlog | At most one cursor per loaded thread (64 total); the single process-wide tap retains 256 Events. Overflow emits `events/resync` with the prior cursor and disables the cursors until re-subscribe, without blocking commits or silently completing missing history. |
| Shutdown grace | 3 seconds total after EOF, input/output failure or a stop signal, covering Run cancellation, Engine closure and output draining; timeout exits 1 with `Run may be interrupted; check Session history`. |

Schema string lengths are character constraints; typed admission additionally
checks UTF-8 byte limits, text/image counts, aggregate image bytes and PNG data.
Paths from clients are absolute; schema validation never replaces Workspace
admission, Provider selection or Guard enforcement.

The OS user's parent process is the principal. State path, Guard executable and
telemetry configuration are fixed before stdin is consumed. Client text/images,
repository content and Tool output are untrusted data. Client configuration can
only narrow the existing Tool grant; repository or model text never grants
trust. Credentials, account records and native keys are absent from the wire.
Existing Goal budgets and per-action/storage bounds apply unchanged; no child
gets broader authority. Startup, path admission, permission asks and inert JSON
encoding form the security pass for this local transport. No socket listener,
authentication, multi-user isolation, encryption, live Provider compatibility or
native platform-support claim is introduced. Linux and macOS use the same loop;
native evidence remains with [NEXT_STEPS](../../NEXT_STEPS.md).

The protocol uses capability strings for feature detection; schema artifact
versioning is separate. Implemented strings are `threads_v1`, `turns_v1`,
`items_v1`, `interrupt_receipt_v1`, `approvals_v1`, `events_subscribe_v1`,
`receipt_get_v1`, `thread_fork_v1`, `turn_steer_v1`, `goal_pause_v1` (the latter also
covers `goal/resume`), `thread_settings_update_v1`, `thread_unsubscribe_v1`, `thread_list_v1`, `thread_rewind_v1`. Breaking wire changes require a new
capability string and at most one release of deprecation compatibility during
pre-beta; stored formats retain their existing no-migration contract.

## Acceptance scenarios and evidence owners

| Scenario | Required observation | Evidence owner |
|---|---|---|
| Stable schema contract | Byte-identical pretty JSON and final newline; intentional golden changes only; canonical Supervisor verdicts validate in raw pages and notifications | [Protocol tests](../../crates/arany-cli/src/protocol/tests.rs), [schema golden](../../crates/arany-cli/tests/fixtures/serve/schema.json) |
| Rewind preview and human confirmation | Tokens bind thread/head/observed states, expire and consume once; default drift refuses, keep restores only the others, overwrite needs separate human confirmation and saves current content; unknown effects block code, bounds refuse before mutation, conversation forks preserve source and budgets, partial receipts stay honest | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs), [protocol conformance and bounds](../../crates/arany-cli/src/protocol/tests.rs), [Guard rewind corpus](../../crates/arany-cli/tests/guard_runs/rewind.rs) |
| Envelope conformance and bounds | Both version forms accepted; malformed, ambiguous and oversized lines rejected; exact 1 MiB boundary accepted | [Protocol codec table](../../crates/arany-cli/src/protocol/tests.rs) |
| Typed conformance | Method, item, notification and reserved approval examples validate against registered schemas; every projected stop has one closed `goal.stopped` value and free text is refused | [Protocol conformance corpus](../../crates/arany-cli/src/protocol/tests.rs), [stop mapping](../../crates/arany-cli/src/cli/serve/items.rs), [projection table](../../crates/arany/src/session/stop.rs) |
| CLI export and refusal | Fresh process output/file bytes and safe target replacement/refusal | [Schema process test](../../crates/arany-cli/tests/generate_schema.rs) |
| Durable start and ordered completion | Exact canonical transcript, schema validation, reopened SessionStarted/MessageAccepted/MessageCommitted/RunFinished and reduced history; resume returns the same items | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs), [transcript](../../crates/arany-cli/tests/fixtures/serve/basic.jsonl), [Engine creation scenario](../../crates/arany/tests/engine_run.rs) |
| Protocol state, capacity and transport recovery | Initialization refusals, 64-thread refusal without eviction, 64/256 queue backpressure, oversized/malformed duplex input recovery, tap-gap sequence and missed count | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs) |
| Cancellation and failure | Busy refusal, interrupt and EOF commit cancellation, three-second grace despite a stalled client, failed accepted turns carry compiled guidance rather than RPC errors | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs) |
| Tool history and human approvals | Reopened dispositions and uncertain unfinished ToolStarted; approval payload equals committed intent and timestamp; accept dispatches once; decline, invalid result, timeout, EOF and cancel deny; late/duplicate/unknown replies change no decision and get no reply; oversized requests deny unsent; full outbound queue cannot prevent expiry or Journal completion | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs) |
| Session lifecycle | Close frees a slot after 64 loaded threads, retains durable history and cancels an active submission before releasing it; list pages include only admitted Workspace metadata, enforce page/line/cursor bounds, reject foreign and invalid cursors and never execute | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs) |
| Live approval settings | Closed mode values and schema-valid result; unknown thread and unsupported/oversized modes refuse; ACP exercises active switches and immutable pending asks | [Serve corpus](../../crates/arany-cli/tests/serve/in_process.rs), [ACP corpus](../../crates/arany-cli/tests/acp/in_process.rs) |
| Steering, pause and resume | Pause and steer are accepted between autonomous Runs (unit test); an exhausted paused Goal's resume closes it Failed in a second server over the same State, proving the pause survived exit; a note is journaled once and refused when empty, over 8 KiB, over the Run budget or over 64 KiB; steer, pause and resume without an active turn or resumable Goal return closed reasons; pause ends the turn `interrupted` and `goal/resume` runs the pinned objective once with one `GoalResumed`; an oversized line is rejected by the line cap | [Serve corpus](../../crates/arany-cli/tests/serve/goal_controls.rs), [protocol bounds](../../crates/arany-cli/src/protocol/tests.rs) |
| Replay, receipts and forks | Exclusive `since`, count/byte pagination, live Event order and JSONL identity; tap overflow recovery; unknown threads/Goals and future cursors refused; receipt equals committed GoalFinished and oversized receipt refuses without closing the connection; fork lineage and Workspace identity preserved, busy and nonterminal sources refused | [History protocol corpus](../../crates/arany-cli/tests/serve/history.rs) |
| Shipped process | Cleared credentials: initialize succeeds, Provider selection fails, EOF exits 0, schema-valid stdout and empty stderr; argument/TTY admission exits 2 before State | [Session process tests](../../crates/arany-cli/tests/session_run.rs) |

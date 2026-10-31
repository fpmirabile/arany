# Agent Client Protocol

## Scope and related contracts

This contract owns `arany acp`, an ACP v1 subprocess on stdio: core turns, guarded Tools, one-use permissions, rich prompts, stored Session load and resume, closure and discovery, live approval modes, budget stop reasons and committed Goal plans. [Serve](./serve.md) owns the shared Server loop, admission, committed projections, cancellation and shutdown; [Session](./session.md), [Goals](./goals.md), [execution](./execution/README.md) and [Tools](./tools/README.md) own Engine policy. ACP only translates the serve vocabulary. Native editor support needs user evidence in [NEXT_STEPS](../../NEXT_STEPS.md).

Arany executes locally instead of delegating to the editor's filesystem or terminal, and sends only the final committed answer, never Provider deltas, because chunks cannot be retracted after a failed validation. The wire subset is checked against the [pinned upstream v1 schema fixture](../../crates/arany-cli/tests/fixtures/acp/README.md).

Audio and model selection are unavailable. Goal pause/resume slash commands and `available_commands_update` are omitted; a slash-prefixed prompt is ordinary user text. Unadvertised requests fail method-not-found; unsupported prompt blocks fail invalid-params.

## Behavior

`arany acp --provider PROVIDER --model MODEL [--effort LEVEL]` pins the same selection as `exec`; `--profile NAME` selects a private roster and refuses a conflicting explicit tuple. Collaboration and maximum children use `exec` defaults and bounds; `--state-dir` selects State. Tools stay off unless `--tools` opts into the private Tool grant, including native Guard availability. No client capability or `_meta` supplies authority. Human-only approvals are unavailable under the [Tools contract](./tools/permissions.md#modes-and-approval-decisions): a human-only Ask sends no permission request, dispatches nothing and records `unavailable_approval`. Host commands run only through an admitted user-owned host-prefix allow rule in default or dontAsk; every other host ask, all of auto and acceptEdits included, is denied, and client approvals add no host authority. [Git hooks](./tools/git.md#git-hooks) consent is terminal-only: ACP uses a trusted folder's saved answer and never asks through `session/request_permission`; with none, hooks stay off and Git write results tell the user to accept them in the terminal.

TTY stdin or stdout is refused before State admission, exit 2 with `error: acp requires piped stdin and stdout`. No terminal UI is initialized. Stdout carries only UTF-8 JSON-RPC 2.0 objects, one per line; diagnostics go to stderr, which ordinary success and missing credentials leave empty. A selected roster prints its bounded manifest on stderr before credential access.

The editor launch directory is not a Workspace, so one containing State does not block startup. Each `session/new.cwd`, and each load or resume against the stored Session, passes full profile/Provider, State-overlap and Workspace-identity admission. Authentication checks current credentials without a Workspace or inference and grants no Run authority. Without credentials, `session/new` fails `auth_required` and creates no Session.

| Method | Input → result | Admission and ordering |
|---|---|---|
| `initialize` | `{protocolVersion,clientInfo?,clientCapabilities?}` → `{protocolVersion:1,agentInfo,agentCapabilities,authMethods}` | First request; a repeat is `alreadyInitialized`. The client version is a u16 integer; unsupported integer versions negotiate to 1, and the client disconnects if it cannot speak 1. Malformed versions fail `invalidParams`. Client info fields pass serve's 256-byte checks. Succeeds without credentials and creates no Session. |
| `authenticate` | `{methodId:"arany-setup"}` → `{}` | Re-probes credentials without inference or Workspace admission. The one agent auth method directs the user to `arany --setup` in a terminal. Missing credentials give `auth_required`; another method ID is invalid. No credential is accepted or returned. |
| `session/new` | `{cwd,mcpServers,additionalDirectories?}` → `{sessionId,modes,configOptions}` | Maps to `thread/start` with the CLI tuple and Tool opt-in. Nonempty additional directories fail. Editor MCP servers are ignored with one fixed stderr notice per connection; none is launched or contacted. Success commits Session creation before replying and counts toward serve's loaded-thread limit. |
| `session/load` | `{sessionId,cwd,mcpServers,additionalDirectories?}` → `{modes,configOptions}` | Maps to `thread/resume` with the same CLI-pinned settings as new, and replays bounded history as updates before replying. Unknown Sessions refuse; foreign Workspaces fail `workspaceMismatch`; active/locked Sessions fail `busy`/`sessionBusy`. |
| `session/resume` | `{sessionId,cwd,mcpServers?,additionalDirectories?}` → `{modes,configOptions}` | Same admission and history-size refusal as load, without history updates. |
| `session/set_mode` | `{sessionId,modeId}` → `{}` | Requires a loaded Session; maps to serve `thread/settings/update`. Unsupported IDs fail invalid-params. Success queues both mode/config updates before replying. |
| `session/set_config_option` | `{sessionId,configId:"mode",value,type?}` → `{configOptions}` | Same as set_mode; returns the complete selector list. Only a string value from the advertised modes is accepted; boolean values/types and other config IDs refuse. |
| `session/close` | `{sessionId}` → `{}` | Requires a loaded Session; cancels active work, withdraws its permission and interrupts the turn. The close response follows the prompt response; until it, new prompts and closes fail `busy`. Close has a 3-second deadline (the shutdown grace); on expiry the turn stays cancelled, the prompt replies `cancelled` with `arany.incompleteUpdates` (or `effectUncertain` when one was observed), nothing is retried and a late completion is ignored. Otherwise the prompt replies `cancelled`, keeping any observed uncertain-effect count. Closed history stays discoverable and loadable. Unknown/unloaded IDs fail resource_not_found. |
| `session/list` | `{cwd?,cursor?}` → `{sessions:[{sessionId,cwd,title,updatedAt}],nextCursor}` | Maps to serve `thread/list` with its default page size, ordering and cursor checks; a page may hold fewer entries, and `nextCursor` continues it. Consumes no loaded slot and never executes. Incoming metadata is never reflected. |
| `session/prompt` | `{sessionId,prompt:ContentBlock[]}` → `{stopReason}` | Requires available selection and a loaded Session. No URI is fetched, opened or turned into authority. Maps to `turn/start`, starting one ordinary Goal. One prompt per process; a concurrent prompt fails `busy`, without queueing. |
| `session/cancel` | Notification `{sessionId}` | Cancels that Session's active prompt, also before its turn starts. Repeated/unknown cancellations do nothing. |
| `$/cancel_request` | Notification `{requestId}` | Same cancellation when the ID names the active prompt; unknown IDs do nothing. |

Capabilities declare `loadSession:true`, image/embeddedContext true, audio false, MCP HTTP/SSE false and `sessionCapabilities:{resume:{},close:{},list:{}}`. Arany never sends `fs/*`, `terminal/*` or `elicitation/*` and never connects editor-supplied MCP servers.

### Closure and discovery

[Serve lifecycle](./serve.md#methods) owns cancellation, slot release, paging and metadata isolation. Omitted cwd selects the launch Workspace; listing never enumerates other Workspaces, and returned cwd is a display string, never a grant. Listings carry no State paths, include paths, saved defaults, credentials or Provider configuration. Titles are neutralized untrusted plain text; editors must render titles, cwd, timestamps and metadata inertly. `updatedAt` is the last committed activity in UTC ISO 8601, omitted when unknown. Discovery authenticates no Provider and loads no Session. A malformed, oversized, stale or foreign-Workspace cursor returns invalidParams. Close never resumes or retries an uncertain effect.

### Approval modes

Following [ACP v1 config options](https://agentclientprotocol.com/protocol/v1/session-config-options), new/load/resume return both `modes` (`currentModeId`, `availableModes`) and one `configOptions` entry with `id:"mode"`, `category:"mode"`, `type:"select"`, `currentValue` and flat `options`, offering exactly `default`, `acceptEdits` and `auto` with the same IDs, order and labels. `--permission-mode` selects the initial value (default `default`); reloading on the same connection keeps the current value. Modes are connection settings, neither journaled nor persisted.

After a successful setter, `current_mode_update` and `config_option_update` (the complete selector list) precede the response; failures send no update and keep the prior value. Reloading while a mode change awaits acknowledgment returns `busy`. A change during a prompt applies to the next approval decision; a pending permission request keeps its intent and is journaled normally.

No mode widens a settings Ask or Deny under the [Tools mode composition](./tools/permissions.md#modes-and-approval-decisions) or the CLI-admitted Tool grant: `auto` cannot enable Tools without `--tools`, expand paths or commands, change Provider selection or budgets, or enable human-only approvals. `plan` and `dontAsk` stay private-settings-only and bypass is always refused. Mode/config IDs are case-sensitive closed values; unknown and oversized strings fail invalid-params. Select values accept an omitted `type`, `type:"id"`, or an unknown string discriminator with a string value, matching the pinned peer SDK's forward compatibility.

### Stored history and continuation

Serve's [Provider setup refusal](./serve.md#error-model) maps to `-32603` with `data.reason:"providerSetupBlocked"` for `session/prompt`, `session/load` and `session/resume`, even for an already loaded Session, appending no Events and making no Provider call.

Load projects committed history in order: each user message as `user_message_chunk`, its Tools as a `tool_call` followed by its terminal `tool_call_update`, then its committed answer as `agent_message_chunk`. Display locations use the client's original cwd spelling. An unfinished Run stays interrupted. The user update's `_meta["arany.turnId"]` and `_meta["arany.turnStatus"]` carry the historical Run ID and serve status; `_meta["arany.images"]` carries image metadata only. Unfinished intents replay as [Uncertain](./execution/events-and-outcomes.md#effect-outcomes); no observation, answer or completion is invented, and replay appends no Events. A later prompt starts a new Goal, never resuming an interrupted one. Resume replays no messages.

A resume result above serve's 1 MiB<!-- unrelated-bound: Serve protocol line cap --> line bound maps to `-32603 contextTooLarge` with no partial replay or loaded Session. Per-update Tool/text truncation applies. Reloading a loaded Session consumes no extra slot.

Text blocks concatenate in order without separators. A `resource_link` becomes `[@name](uri)`. An embedded `resource` must contain `uri` and `text`; blob resources reject. It becomes `<context ref="ESCAPED_URI">BODY</context>`, escaping `&`, `<`, `>` and `"` in the URI and replacing with `&lt;` the `<` of every ASCII case-insensitive `</context` prefix in the body. These wrappers remain user data, never instructions or trust. PNG images require `mimeType:"image/png"` and base64 `data`; an image `uri` is ignored, and image-only prompts are accepted. Text and images follow serve's admission and the [terminal image contract](./terminal/drafts-and-paste.md#paste-and-attachments). Other content types and overflow reject the whole prompt with invalid-params.

Budget-stop prompt results keep their `max_tokens` or `max_turn_requests` reason and add `_meta["arany.budgetRefusal"]` (the committed record, or null) and `_meta["arany.status"]` (the shared Arany status); loaded history carries the same fields on its user-message update without a fabricated assistant message. These follow the [Goal refusal contract](./goals.md#budget-refusal-feedback).

### Tool updates and permissions

A permission request refers to a committed `ToolProposed` intent and may precede the Tool's started update; proposals that never start create no historical Tool item.

Only committed serve items produce Tool updates. `item/started` becomes `tool_call` with the immutable intent ID, compiled title plus bounded escaped intent fields, kind, pending status and locations. Command calls are `execute`, file changes `edit` (delete-only sets `delete`), Read/Skill/typed Git reads `read`, List/Search `search`, GhPrList/GhPrView `read`, WebFetch `fetch`, WebSearch `search`, and MCP/other calls `other`. GhPrList and GhPrView titles name GitHub pull-request listing or inspection with escaped repository detail, without a Workspace location. Move is `edit` with the `Change files` title and both source and destination locations. Journaled call fields form `rawInput`; file-change items expose paths without content or diff bytes. Locations are display-only, never used for access.

`item/completed` becomes `tool_call_update`: success is `completed`; declined and failed dispositions are `failed`, naming the disposition in fenced content. An [Uncertain](./execution/events-and-outcomes.md#effect-outcomes) outcome is `failed` with `_meta["arany.status"]:"uncertain"` and the text: "Outcome unknown: Arany could not confirm whether this effect happened. It was not retried; inspect the Workspace before continuing." A terminal status is never replaced with pending. Tool output is escaped inside a code fence longer than every backtick run in it. There are no client terminals and no diff contents.

A recoverable denial or known Tool failure completes its update as failed and reaches the model once as the Tool result, without hook and permission-rule prose; the prompt may continue, and its final stop reason comes from the committed Engine outcome.

A serve approval request becomes `session/request_permission` with the Session ID, a pending `toolCall` update, its exact committed EffectIntent in `rawInput`, escaped approval lines, and exactly these ordered options:

| optionId | kind | name |
|---|---|---|
| `deny` | `reject_once` | Deny |
| `allow_once` | `allow_once` | Allow once |

`allow_once` maps to serve `accept`, `deny` to `decline`; an unknown option, malformed result or client error records unavailable approval, and a cancelled outcome maps to `cancel`. A reply resolves only its one outstanding request; unknown, duplicate, withdrawn and cancelled IDs change nothing. `allow_always` is never offered, and `reject_once` is an option kind, not an accepted ID. ACP cannot attest a human gesture, so human-only requests follow the [Tools contract](./tools/permissions.md#modes-and-approval-decisions). No option creates host authority or a remembered rule; all Policy, one-use grant, journal and Guard checks still apply.

Cancellation, Tool completion or withdrawal retire the pending permission and send `$/cancel_request` for it if it was sent. A primary permission has no wall-clock expiry. A withdrawn request cannot be authorized by a late reply. Child-request withdrawal follows the [Tools approval rules](./tools/trust-and-approvals.md#folder-trust-and-approvals). An oversized permission or a full/closed output queue records unavailable approval without sending or truncating it, and is never retried.

A committed answer produces exactly one `session/update` with `update.sessionUpdate:"agent_message_chunk"` and `content:{type:"text",text}`. User input is not echoed; previews, commentary and uncommitted text are absent. The prompt response follows all its updates. Text passes through as model markdown; the editor owns inert rendering.

### Goal plans

With Tools enabled, a committed `GoalPlanContractRecorded` for the active Run produces a full replacement `plan` of two entries, the mission then the success criterion, each with priority `medium`; the mission starts `in_progress` and the criterion `pending`. Only committed Goal completion marks both `completed`; failure or interruption returns both to `pending`, since ACP has no failed status. A Goal without a contract gets no plan. Load/resume resume no old Goal or plan.

Plan content is untrusted plain text: control/bidi characters are escaped, and paths and markup are never resolved, executed or treated as instructions; editors must render entries inertly. Exact-content criteria name the file without the expected bytes; Tool criteria show the journaled call as data. Plans never change grants or budgets.

Missed updates are recovered from committed Events before the prompt response, without duplicates. If recovery is refused or interrupted by shutdown, the response carries `result._meta["arany.incompleteUpdates"]:true` and stderr one fixed diagnostic. A new process never resumes or retries a Goal implicitly.

The prompt result maps serve's closed `goal.stopped` projection:

| Committed outcome | Prompt result |
|---|---|
| Run finished, Goal completed and no `goal.stopped` | `stopReason:"end_turn"` |
| Client cancellation or process stop, or `goal.stopped` `paused` or `cancelled` | `stopReason:"cancelled"`, including a teardown error after cancellation; `_meta["arany.uncertainEffects"]` reports observed uncertain Tools when nonzero |
| Failed Run (`goal.stopped` `failed`, `notAdmitted` or absent), uncertain effect (an observed uncertain Tool or `goal.stopped` `uncertain`), interruption without a terminal receipt (`interrupted`) or hook stop (`stoppedByHook`) | `-32603` with compiled recovery guidance and `data.reason:"turnFailed"`, `"effectUncertain"`, `"turnInterrupted"` or `"stoppedByHook"` |
| Budget stop (`goal.stopped` `goalExhausted`) | `limit:"tokens"` maps to `max_tokens`; `calls`, `runs` and `activeTime` map to `max_turn_requests`. Cancellation keeps `cancelled`, observed uncertain effects keep the failure path, and an ordinary failure at a budget boundary is not exhaustion. An admission failure without a committed turn stays an RPC error. |

Failure is never `end_turn`; `refusal` is unused. A submission without a committed terminal Run keeps serve's fatal Session-history recovery behavior. EOF, input/output failure and SIGINT, SIGTERM or SIGHUP cancel work within serve's shared grace; clean EOF exits 0, transport/storage errors or grace expiry exit 1.

Arany's closed Event vocabulary belongs to [serve](./serve.md#framing-and-envelope).

### Errors and envelopes

Envelopes require `jsonrpc:"2.0"` and reject batches, unknown envelope fields, non-integer IDs and ambiguous shapes. IDs are signed i64 integers or strings of at most 256 bytes, echoed unchanged; an oversized ID gets a null-ID invalid request. Unknown params, `_meta` fields, notifications and unsolicited responses are ignored and authorize nothing.

Two divergences keep typed admission: the peer Rust SDK reads a string `protocolVersion` as v0 and answers 1, whereas Arany requires a u16 integer and refuses strings with `-32602 invalidParams`; and although the ACP schema allows null request IDs, Arany refuses them with `-32700 parseError`. Null IDs remain valid in error responses.

| Code | Meaning and reasons |
|---|---|
| `-32700` | Malformed JSON or envelope, including a null request ID; `parseError` |
| `-32600` | `notInitialized`, `alreadyInitialized`, `messageTooLarge`, `invalidRequest`, `busy`, `threadLimit` |
| `-32601` | Unsupported method |
| `-32602` | Invalid params or unsupported content; `invalidParams` |
| `-32000` | `auth_required`, with terminal setup guidance |
| `-32002` | Unknown/unloaded Session after authentication; `resource_not_found` |
| `-32603` | Serve Engine/selection failures retain their closed reason; accepted turn failures use the outcome table above |

Serve `providerUnavailable` selection failures become `auth_required`. Other serve `-32000`/`-32001` errors become `-32603`, preserving their reason and message.

## Limits and compatibility

Shared bounds belong to [serve limits](./serve.md#limits-and-compatibility).

| ACP boundary | Bound and overflow |
|---|---|
| Incoming/outgoing line including newline | Serve's 1 MiB<!-- unrelated-bound: Serve protocol line cap -->. Incoming overflow is discarded through newline, then null-ID `-32600 messageTooLarge`; malformed JSON gets `-32700`. The next line remains usable. |
| Reader buffer / serve inbound | 64 KiB<!-- unrelated-bound: protocol reader buffer --> / 64 messages with backpressure. |
| Serve outbound / ACP outbound | 256 messages each, plus at most 256 deferred serve messages; deferred overflow fails the connection with Session-history recovery guidance. Permission requests and withdrawals never wait behind it. If replies fill ACP output, input fails closed and starts shutdown rather than holding cancellation behind stdout. |
| Pending client requests | 61; excess requests fail `busy`. |
| Sessions / active prompt | Serve's 64 / 1, with `threadLimit` / `busy`, no eviction. Close frees a slot after cancellation settles. |
| Prompt blocks / aggregate text after rich-content conversion | 64 / [64 KiB](./limits.md#max_user_message_bytes) UTF-8, including mention/context syntax; excess or empty/whitespace-only text without images fails `invalidParams`. |
| Images | [Serve](./serve.md#limits-and-compatibility) and [terminal](./terminal/drafts-and-paste.md#paste-and-attachments) image limits; overflow rejects without truncation. |
| Session list | Serve default page size; no client page-size option. |
| Mode selectors | One config selector and exactly three mode values; invalid values refuse without changing state. |
| Tool content / title | 64 KiB<!-- unrelated-bound: ACP Tool content projection --> / 256 bytes of escaped text plus a Session-history truncation marker. If the line still exceeds the cap, optional raw input/output and locations are omitted and content becomes a history marker. |
| Goal plan | Two entries, one revision per prompt; each entry 2,048 UTF-8 bytes of escaped content plus a history marker. |
| Pending permissions | One; no primary wall-clock expiry. |
| Oversized answer projection | Cut at a UTF-8 boundary to at most 128 KiB<!-- unrelated-bound: ACP answer projection --> text plus `[truncated; see arany show SESSION_ID]`. Other oversized projections fail closed. |
| Reconciliation | Serve's Event pages and Session Event cap; at most 10,000 Tool-phase deduplication entries per prompt, whose overflow suppresses further Tool updates and marks incompleteUpdates. |
| Load/resume history | [Stored history and continuation](#stored-history-and-continuation) and [serve's Session Event cap](./serve.md#limits-and-compatibility). |
| Shutdown | Serve's total three-second grace. Timeout reports `Run may be interrupted; check Session history`. |

Linux and macOS behave the same, with no shell or service-manager dependency. Path text never grants authority. ACP adds no socket listener, editor authentication, multi-user isolation, encryption, Provider call or retry path, child authority or credential channel, and changes no Goal budget, State storage or per-action limit.

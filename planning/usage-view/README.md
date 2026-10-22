# Terminal usage view

## Metadata

- Status: `In progress` — U1 and U2 implemented; U3 is next.
- Owner: Fernando Mirabile
- Last updated: 2026-10-22
- Remaining scope: U3 API/custom rate limits.

## Outcome and scope

One feature: inspect Provider-reported usage in Arany's terminal, with the same
facts available after reopening the conversation. Extend `/status` with usage
and add `/usage` for the focused report. Show input/output tokens and reported
cache counters for every admitted route; show ChatGPT weekly and reported
5-hour consumption and resets; show API rate limits only when reported.
This serves the harness's responsibility to explain execution and consumption.

Use existing authorized inference responses and committed history. Opening the
view makes no request, reads no credential, creates no Session and writes no
Event. No polling endpoint, new account integration, dollar estimate, billing
dashboard, WebSocket transport, dependency or change to Goal enforcement is
part of this feature. The broader [Goal activity proposal](../goal-progress/README.md)
can consume this projection; it is not a prerequisite or part of this plan.

## Research

### Arany contracts and current flow

The [Provider usage contract](../../docs/specs/provider.md#usage-provenance-and-reliability)
owns token normalization, strict required usage, unknown failures and bounded
retry behavior. [ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs)
admits the official direct-use route only, without API-key fallback.
[Session persistence](../../docs/specs/session.md#persistence-facts) owns replay;
[terminal output](../../docs/specs/terminal.md#conversation-progress-and-history)
owns local measurements and inert accessible presentation. These are the
contracts for U1 token reporting and the remaining limit-observation slices.

Existing [Provider usage types](../../crates/arany/src/provider.rs) and
[call/compaction records](../../crates/arany/src/session/events/records.rs)
already carry the four token counters. The [primary loop](../../crates/arany/src/engine/run_loop.rs)
commits before continuation; [compaction](../../crates/arany/src/engine/compaction.rs),
[in-Run compaction](../../crates/arany/src/engine/run_loop/tool_compaction.rs) and
[routing](../../crates/arany/src/engine/routing.rs) identify the remaining fact
owners. The [agent](../../crates/arany/src/session/reducer/agent.rs) and
[Goal](../../crates/arany/src/session/reducer/goal.rs) reducers accumulate total
and cache-read tokens with uncertainty, and the
[views](../../crates/arany/src/session/reducer.rs) now retain U1's separate-counter
usage projection, including in-Run compaction usage.

[Subscription](../../crates/arany/src/provider/openai/subscription.rs) now collects
bounded passive plan observations through every attempt owner;
[custom transport](../../crates/arany/src/provider/custom/transport.rs) still has no
normalized API-limit snapshot; [HTTP failures](../../crates/arany/src/provider.rs)
and [retry records](../../crates/arany/src/provider/routing.rs) retain limited
retry guidance. The [registry](../../crates/arany-cli/src/terminal/commands.rs)
exposes `/usage`; [attached `/status`](../../crates/arany-cli/src/cli/attached/controls.rs)
opens that report with Session/Run and local-context facts,
[agent details](../../crates/arany-cli/src/presentation/agents.rs) show last-call
tokens, and [Goal presentation](../../crates/arany-cli/src/presentation.rs) shows
known totals. Slice sections below name the evidence owners to extend.

### Peer and API evidence

Checked 2026-10-22; source inspection only, with no credentials or paid probes.
Codex source is pinned to `a390571745da105cfdc85443dd1d38bf78e5ea6e`.
Live spot-check of the pinned parser, WebSocket consumer and status constant,
plus Claude Code's release notes, confirms the four claims below.

**Codex acquisition.** Its HTTP/SSE adapter parses response headers before
consuming the stream and emits `ResponseEvent::RateLimits`
([adapter](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/codex-api/src/sse/responses.rs#L38)).
The default family is `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}`:
finite percentage, optional integer minutes and optional Unix epoch seconds.
`codex.rate_limits` JSON uses `rate_limits.primary/secondary` with `used_percent`,
`window_minutes` and `reset_at`
([parser](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/codex-api/src/rate_limits.rs#L57)).
Its observed event consumer is the
[WebSocket adapter](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/codex-api/src/endpoint/responses_websocket.rs#L859);
this does not establish such events on Arany's SSE route. Codex also separately
reads `/wham/usage` or `/api/codex/usage`
([backend reader](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/backend-client/src/client/rate_limit_resets.rs#L69)).

**Codex display.** Window duration determines 5h/weekly labels
([labels](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/tui/src/chatwidget/rate_limits.rs#L103)).
`/status` distinguishes missing, unavailable and stale data, using a 15-minute
staleness threshold
([status state](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/tui/src/status/rate_limits.rs#L52)),
and renders percent remaining plus reset information
([card](https://github.com/openai/codex/blob/a390571745da105cfdc85443dd1d38bf78e5ea6e/codex-rs/tui/src/status/card.rs#L501)).
Arany uses percent **used**, as explicitly requested by its owner.

**Authorized-route boundary.** The official direct-use contract names
`POST https://api.openai.com/v1/responses` and excludes ChatGPT backend endpoints
([models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference));
it requires streaming and `store: false`
([preview requirements](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)).
These sources establish neither Codex limit headers/events nor a public quota
read on this route. A usage 429 can mean an app-specific cap, not total plan
exhaustion or a known reset
([errors](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery#structured-responses-errors)).
Consequently Arany's passive acquisition is conditional: missing weekly data is
an expected unknown state, not permission to use Codex's backend endpoint.

**Claude Code.** Current `/usage` combines session usage, plan limits and activity;
`/cost` and `/stats` are aliases, merged in v2.1.118
([commands](https://code.claude.com/docs/en/commands),
[release](https://github.com/anthropics/claude-code/releases/tag/v2.1.118)).
It separates input/output/cache-read/cache-write, estimates cost locally, and
can retain a failed-fetch snapshot under 60 minutes old with its age
([usage guide](https://code.claude.com/docs/en/costs#using-the-usage-command)).
Its status-line data exposes independently optional five-hour/seven-day used
percentages and epoch resets after a response, removing expired windows
([status-line fields](https://code.claude.com/docs/en/statusline#available-data)).
Copy the focused usage entry, counter separation and explicit unknown/stale
states. Arany needs no cost calculator or `/cost` alias for this request.

| Route | Reported fields |
|---|---|
| Responses, including ChatGPT | `usage.input_tokens` / `output_tokens`; `input_tokens_details.cached_tokens` and documented `cache_write_tokens` are input subsets. Arany's normalization is defined by the [Provider usage contract](../../docs/specs/provider.md#usage-provenance-and-reliability). [Responses usage](https://developers.openai.com/api/reference/python/resources/responses/methods/create), [cache semantics](https://developers.openai.com/api/docs/guides/prompt-caching). |
| OpenAI rate information | `x-ratelimit-{limit,remaining,reset}-{requests,tokens,project-tokens}` when present. Reset values are relative durations such as `1s` or `6m0s`; normalize against receipt time. Project tokens is a fixed dimension label, not a project identifier. `Retry-After` is separate retry guidance, not a plan reset. [Official headers](https://developers.openai.com/api/docs/guides/rate-limits#rate-limits-in-headers). |
| Messages | `input_tokens` excludes cache reads/writes; final usage combines `message_start` with cumulative `message_delta.usage`. [Streaming](https://platform.claude.com/docs/en/build-with-claude/streaming), [cache categories](https://platform.claude.com/docs/en/build-with-claude/prompt-caching). |
| Anthropic rate information | `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-{limit,remaining,reset}` and `anthropic-priority-{input-tokens,output-tokens}-{limit,remaining,reset}` when present. Resets are RFC3339; remaining tokens may be rounded to thousands and generic tokens reports the most restrictive applicable limit. Preserve that precision and qualification; do not calculate exact spend or consumption percentage. [Official headers](https://platform.claude.com/docs/en/api/rate-limits#response-headers). |
| DeepSeek / GLM | Their separate Chat Completions references expose prompt/completion and cached input, without establishing Messages counters or plan windows: [DeepSeek](https://api-docs.deepseek.com/api/create-chat-completion/), [GLM](https://docs.z.ai/api-reference/llm/chat-completion). Arany's [admitted routes](../../docs/research/deepseek-kimi-glm-providers-2026-10.md) use Messages; compatibility cannot prove an optional field is present. |

Peer divergence is limited to the guarantees and explicit owner scope: commit
before display, no inadmissible quota endpoint, no inferred allowance, and used
rather than remaining percentages. Retain expired historical observations with
an unmistakable stale label for durable replay; do not present them as current.

## Contract and scenarios

### Scope, units and interaction

- `/usage` takes no arguments and is read-only while idle or running. `/status`
  preserves its Session/Run/context information and opens the same usage detail
  surface. Registry help and completion expose the command without network I/O.
- Report this Session's local calls, grouped by actual pinned Provider/profile
  and model, across primary, children, review and compaction. Distinguish the
  current/latest Run from Session totals. Resolve fallback provenance from
  committed facts, never from today's defaults. Show inherited fork history
  separately and exclude it from the new Session's consumption totals; resume
  continues the original totals. Standalone `provider check` calls are outside
  Session totals and remain with their diagnostic journal owner.
- Input includes cached subsets; cache reads/writes are labelled `included in
  input`, never added again. Output includes all Provider-reported output,
  including reasoning where the Provider includes it. Do not derive billable
  cost, a token allowance or context capacity from these counters.
- Track completeness per counter: `12,340 known; some calls unreported` is a
  partial sum, not a complete total. If no call reported a counter, show
  `unknown (not reported)`. No calls is `no calls yet`, not a remote quota of
  zero. Pending reservations and failed retries qualify totals as incomplete.
- Plan usage is an account-level server observation, not this Session's share.
  Never sum percentages across models, roles, concurrent calls or Sessions.
  Bind a snapshot to its existing call/Run provenance. Display no new account
  identifier; separate historical route selections using local Run labels.
  A changed selection never inherits a previous selection's live-looking limit.
- Render tokens with grouped decimal counts and percentages with `%`. Use the
  shared presentation `format_bytes` convention from
  [PR #300](https://github.com/fpmirabile/arany/pull/300): base-1024 scaling,
  B below 1,024 bytes and one decimal place with KB/MB/GB/TB labels above it
  (1,024 bytes = `1.0 KB`). Follow its [binary practice](../../agents/arany-cli.md)
  formatter rule when integrated; preserve `local request budget; not model tokens`.
  Tokens remain counts and canonical bytes remain exact. Use the shared
  duration formatter for relative resets, such as `resets in 2 h 15 min`;
  details also give the absolute UTC reset timestamp.
- Fullscreen and Classic reuse the scrollable detail/selector ownership and
  input barriers used by agent inspection. Escape restores draft and caret,
  pinned by [`inline_quick_selector_returns_to_the_same_draft_and_caret`](../../crates/arany-cli/tests/session_run/slash_completion.rs#L1832),
  which opens the inspector over a Unicode draft, closes it with Escape and
  verifies insertion at the retained caret. Keep the history anchor and Ctrl+C
  cancellation; extend that fixture for usage inspection in U1.
  Linear/screen-reader output prints the same
  labelled facts without boxes, color dependence, control escapes or rewrites.
  Use a full detail/report path, not the 240-cell linear notice path. At narrow
  widths wrap fields vertically; scope, unknown and stale labels remain whole.
- Refresh projections on committed Events or explicit inspection, not an idle
  timer. Opening while running includes the current committed watermark and
  says that in-flight usage is pending. Update the terminal spec's existing
  submission-boundary wording specifically for this usage projection.

### Freshness and failure

Every limit is labelled `last reported`, with local observation time and age.
Age describes a snapshot, never a guarantee that no other client consumed it.
Read the observation clock exactly once when each header/event sample is
received; store that `observed_at` value in its canonical Event at settlement.
Do not replace it with stream-completion or append time. Replay and rendering
read only the stored observation value; neither regenerates it. Select
newest by receipt time with committed sequence as tie-breaker so a slow older
call cannot overwrite a newer concurrent observation. A backward/future clock
anomaly produces `age unknown`, not a fresh claim.

A later response that omits limits does not fabricate zero or erase a prior
valid sample: display the old sample with `latest call did not report limits`.
An invalid optional sample similarly adds a closed unavailable reason. After
its reported reset passes, label the sample `stale; reset time passed, current
usage unknown`; do not reset its percentage to zero or advance its window.
On reopen all snapshots are historical, with the same observation/reset times.
Also label samples older than 15 minutes stale, following Codex; this is a
display threshold, not remote expiration. Without a reset time, display
`reset unknown`. Replay never reads a clock. For relative age/reset formatting,
the terminal edge supplies an explicit current-time value on an ordinary draw
or invocation; the renderer reads no clock and compares that input with stored
timestamps. Deterministic `show` uses only stored absolute times.

The weekly row is always present for ChatGPT, including `unknown (not reported)`.
Show a 5-hour row only when that window was actually reported. Name windows from
their reported duration: 300 minutes is `5-hour`, 10,080 is `weekly`; primary and
secondary are not unconditional duration aliases. Other durations retain their
actual label. A percent without duration must not be relabelled weekly. Absent
percent or reset stays unknown independently. API rate limits remain requests
or tokens per reported window; they are not subscription allowances. A 429 or
usage-limit reason alone supplies no percent, balance or reset.

Malformed required usage, stream framing, credential reflection and existing
response-contract failures keep their current rejection semantics. Optional
limit metadata cannot turn an otherwise rejected response into success or change
retries, fallback, Goal admission, permissions or cancellation. Valid header
observations can survive a subsequent body failure; token usage remains unknown.
No response means no new snapshot. A crash before commitment leaves no durable
sample and never causes a network replay or automatic refresh.

## Chosen direction

Ponytail choice: reuse semantic Provider adapters, the Engine's attempt owners,
Session reducer and pure CLI presentation. Extend the existing in-process
Provider seam, justified by native OpenAI, subscription, Messages and exact
custom adapters. HTTP header maps, arbitrary JSON, credentials and vendor types
never cross into Engine policy. No new crate, daemon, persistent cache or trait
for a single implementation.

The adapter translates a closed allowlist into optional numeric facts, with
source (`headers` or recognized event), observation time and availability state.
Use a bounded per-attempt observation slot owned by the caller so an admitted
header sample survives a dropped future, timeout or body error. It is internal
collection, not a display observer; the UI sees only committed facts. A valid
recognized event replaces the same window's header sample; omitted windows keep
their own previous source/time. Admit only the default Codex meter (absent meter
name or exact `codex`); other meter names cannot overwrite it and are not stored.
Unsupported event shapes grant nothing. Partial updates never combine fields
from different samples into a fabricated complete window.

At attempt settlement the Engine drains this slot into that attempt's existing
fact: `ProviderCallRecorded`, `ProviderRetryScheduled`, `ProviderFallbackUsed`,
`ContextCompacted` or `GoalContextCompacted`. Retry/fallback snapshots describe
the failed source attempt, never the replacement. Commit before retry waiting,
replacement dispatch, Tool/agent continuation or terminal publication. An append
failure stops continuation and publishes no uncommitted usage. Every path must
carry this, including children, review and both compaction loops; success-only
response fields cannot cover it.

Keep at most one final merged snapshot per attempt, not an Event per stream
update. Draining consumes it: cancellation during a retry wait cannot copy the
previous attempt's sample into another call record. This observes response
metadata without changing the billing outcome.
The reducer validates call ownership, source, field bounds and ordering, then
updates a bounded derived report. Counts fold each canonical call/compaction
once; Goal reports and copied summaries are not additional consumption. Existing
retry/fallback accounting and unresolved reservations supply incompleteness.

Use an additive optional metadata field omitted when empty under the
[Event compatibility rule](../../docs/specs/execution.md#admission-persistence-and-recovery):
old bytes/digests remain valid and mean unreported, with no historical backfill.
If implementation requires a required field or changes existing meaning, stop
and update this plan with the affected payload version, migration step and
fixture before proceeding. Do not introduce a compatibility reader. Include
old-history/fork byte-preservation and non-default strict-decoding fixtures in
the slice introducing the field. Review decoder/admission check-version bumps
at the [Provider owner](../../docs/specs/provider.md#prompt-caching).

Linux and macOS share all proposed parsing, journaling and presentation
semantics; there is no native adapter change. Synthetic tests do not establish
live header availability, emulator interoperability or screen-reader usability.

## Security and resource model

Triggered passes: external-response parsing, durable format/projection and
untrusted rendering. Assets are credentials, private history, correct usage
accounting and terminal integrity. Authority remains the already admitted
immutable Provider request, selected credential and destination; inspection
adds no egress, grant, child authority, startup discovery or credential lookup.
Admitted State and existing platform protections remain required before history
access. Provider-supplied metadata is untrusted data, never a configuration,
permission, enforcement receipt or billing guarantee.

Persist only bounded numbers, closed enum labels and timestamps. Do not persist
raw headers, token strings, account IDs/emails, organization/project IDs, plan
names, bucket names, URLs, response bodies or upstream error text in the new
snapshot. Associate through existing call provenance rather than copying or
hashing new account identifiers. Preserve selected-credential reflection guards
before publication, including header values. Human output uses compiled labels
and existing inert renderers; unknown header names and metadata are discarded.
Snapshots never enter Provider prompts, compaction input or telemetry content.

| Surface | Proposed bound and overflow behavior |
|---|---|
| Header extraction | Fixed allowlist, at most 32 recognized header values, 128 bytes each and 4 KiB aggregate; duplicate recognized names or invalid/oversized values make the affected sample unavailable; never retain arbitrary headers |
| Snapshot | Two plan windows and at most eight API limit dimensions, at most 4 KiB serialized per attempt; extra/invalid optional observations are unavailable with a closed reason, not silently relabelled |
| Numeric facts | Finite percentage 0–100; positive window minutes fitting `u32`; counts fitting `u64`; remaining cannot exceed a supplied limit; resets must fit supported UTC timestamp arithmetic; invalid fields stay unknown, never clamp |
| Stream collection | Existing 1 MiB/8,192-event/depth limits remain; one bounded slot per admitted concurrent call, newest valid sample per window/dimension, no queue or new task; malformed framing still rejects |
| Persistence | Existing Event payload, Session lineage and Store page/headroom bounds remain; include the extra 4 KiB in action byte reservations before dispatch; an Event overflow refuses append and continuation |
| Derived report | At most one group per distinct pinned route in the existing 10,000-Event lineage cap, checked `u64` totals; build during validated reduction/reload, not on every frame; no unbounded cross-Session aggregation |
| Rendering | At most 16 route groups and 8 KiB escaped text per page with a visible range and next/previous controls; no silent loss of groups; existing geometry, Unicode and input bounds apply |
| Egress/retry/cost | Zero added calls, refreshes, retries or paid probes; existing per-call deadlines and shared Goal reservations remain authoritative |

The UI describes observed remote claims, not an independently verified balance.
Private SQLite remains sensitive and unencrypted under its existing contract.
No raw metadata is logged on parse failure. Tests use synthetic canaries, never
production header captures or accounts.

## Vertical slices

### U1 — Inspect reported token usage for every admitted Provider

Implemented. Current token mapping, scope and interaction live in the
[Provider](../../docs/specs/provider.md#usage-provenance-and-reliability),
[Session](../../docs/specs/session.md#persistence-facts) and
[Terminal](../../docs/specs/terminal.md#reported-usage) contracts.

### U2 — Show ChatGPT plan windows from committed response observations

Implemented. Passive acquisition, bounded samples and all-attempt settlement now live in the
[Provider](../../docs/specs/provider.md#passive-plan-limit-observations),
[ChatGPT](../../docs/specs/chatgpt.md#catalog-checks-and-runs),
[Execution](../../docs/specs/execution.md#admission-persistence-and-recovery),
[Session](../../docs/specs/session.md#persistence-facts) and
[Terminal](../../docs/specs/terminal.md#reported-usage) contracts. U3 reuses this path.

### U3 — Show API and custom-route rate-limit facts where exposed

One PR completes the same report for API-key OpenAI/Anthropic and admitted
custom protocols. Existing HTTP response → protocol-specific allowlist parser →
U2 attempt commitment/reducer → labelled requests/tokens remaining/limit/reset
is the complete path. Show `unknown (not reported)` for unsupported quotas;
never translate vendor compatibility into a subscription allowance or add a
balance endpoint. Update Provider and Terminal contracts, and other owners only
if their accepted format or behavior changes.

Extend the [Responses corpus](../../crates/arany/src/provider/openai/wire/tests.rs),
[Messages corpus](../../crates/arany/src/provider/anthropic/wire/tests.rs),
[custom checker](../../crates/arany/src/provider/custom/check.rs),
[custom process journey](../../crates/arany-cli/tests/session_run/custom.rs) and
[native HTTPS fixtures](../../crates/arany-cli/tests/session_run/setup/offline_https/anthropic.rs).
Verify relative OpenAI reset durations versus absolute Anthropic reset dates,
partial dimensions, multiple token buckets, retry guidance, success/429/5xx,
negative/overflow/inconsistent counters and unknown custom headers. DeepSeek/GLM
fixtures use only their admitted Messages shape, not Chat Completions usage
aliases; absent limit data is an explicit successful display case. Reuse U2's
durability and cancellation scenarios through these real adapter boundaries.

## Risks and mitigations

- Codex's endpoint contract differs from Arany's: passive fields remain
  conditional; no private endpoint or token import can fill a missing window.
- Shared account activity can change immediately: show observation age/scope;
  never promise remaining capacity or alter Goal admission from a snapshot.
- Metadata on failed or cancelled calls can be lost by success-only plumbing:
  the caller-owned slot and all-attempt settlement tests are required in U2.
- An expanded view can truncate away caveats: use paged detail output and
  exact linear/monochrome/narrow fixtures, not ordinary short notices.
- Existing history has no limit timestamps: omission means unknown; do not
  infer historical rates, fabricate samples or rewrite old Event bytes.

## RPI gates and exit criteria

- **Research gate:** source-verified local flow and cited peer/API fields;
  distinguish Codex backend behavior from Arany's admitted direct-use route.
  Live absence is a supported state, not a dependency on a paid probe.
- **Plan gate:** each slice owns a usable observation, all material choices
  resolve from sources and existing guarantees, and only residual owner-only
  questions may block it. Implementation requires a subsequent owner request.
- **Implement gate per slice:** update the owning contracts once, then pass
  formatting, targeted Clippy and the existing focused evidence owners plus
  complete CI for code changes under [verification](../../docs/development.md#verification).
  If a source or implementation contradicts the design, stop and revise this
  plan before continuing. This documentation phase runs no Cargo tests.
- Synthetic process evidence compares exact human output, serialized JSONL,
  closed/reopened SQLite Events and replayed views. Tests prove no credential
  access or Provider dispatch on inspection/replay, journal-first publication,
  strict bounds and unchanged retry/Goal behavior. Frame/linear coverage spans
  80, 50, 40 and 16 columns, no-color and screen-reader modes, active/idle state,
  resize and draft restoration; existing PTY owners cover interaction.
- Before a live/native support claim, place the remaining direct-use header,
  custom-route, Linux/macOS emulator and assistive-technology checks with their
  procedures in [NEXT_STEPS.md](../../NEXT_STEPS.md), using existing evidence
  sections. Do not claim these checks ran or block ordinary synthetic delivery.
- On feature completion follow the [planning lifecycle](../README.md#lifecycle):
  accepted behavior lives in specs, unresolved evidence in NEXT_STEPS, and the
  plan/index row are removed in the closing change.

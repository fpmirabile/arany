# Development diagnostics and responsive model browsing

## Metadata

- Status: `Done` (local implementation and compatibility follow-up verified; uncommitted)
- Owner: Arany maintainers
- Last updated: `2026-10-04`

## Goal and scope

Make subscription failures locally diagnosable without collecting secrets or conversation data, and keep chat/model selection responsive while catalog work is in flight. Correct metadata-only rejection of otherwise valid subscription SSE without relaxing the stream or outcome contract. No live calls, commits, new billing route, or manual-testing prerequisite.

## Current state

Canonical subscription failures retain their existing coarse categories, while development diagnostics distinguish the local rejection stage. Setup seeds Composer's bounded same-source catalog; `/model` opens it immediately and refreshes within the input-owning picker. Preparing and committed activity retain separate terminal owners, with an explicit waiting state before an accepted answer.

## Ownership and seams

The private diagnostic module owns closed local failure metadata and bounded backtraces; StateRoot owns checked file admission. CLI enables the concrete facility only for a debug product process after ordinary Session/Run admission. This is not an Engine behavior seam or canonical state. The subscription adapter owns detailed protocol stages without changing canonical failure categories. Composer's existing Provider/account-scoped catalog remains the sole cache; CLI owns one cancellable refresh future, and Terminal owns its visible state.

## Chosen direction

Use a private, bounded, lossy local debug log, not raw HTTP logging, a global stderr subscriber, or a second telemetry framework. Reuse the current in-memory catalog rather than hardcoding plan-wide availability or introducing a persistent cache schema. Open cached choices immediately and refresh while the picker owns input; preserve row identity and tentative effort when replacing rows. Fresh processes still need their first account-scoped discovery. Custom profiles retain current local declaration loading.

## Security and resource model

- Debug defaults only; optimized builds do not enable local logging. No diagnostic output on process stdout/stderr, helpers, help/version or read-only `show`.
- Log only compiled stages and bounded numeric counters. Never format an upstream error/body, request, token, prompt, result, account identity or environment. A stack trace shows local call sites, not an async causal trace or upstream stack; source locations and controls are omitted.
- One checked owner-only, no-follow, single-link file under admitted State outside Workspace, capped at 256 KiB; rotate by truncation at the cap. A record is capped at 24 KiB, including its stack. Nonblocking file/process locks drop concurrent diagnostics; logging failure cannot change a Run result.
- Existing catalog row/ID/effort bounds and source invalidation remain. No discovery on ordinary keystrokes, no inference, retries or credential fallback. Dropping an already delegated renewal waiter does not guarantee rollback.
- Refresh updates only at an accepted input boundary. Selection remains staging; Run independently revalidates credentials, consent and strict outcomes.

## Execution and verification map

1. Extend the existing subscription protocol corpus with precise failure-stage expectations; prove malformed/incomplete streams still reject and valid framing still accepts. Add the bounded file/privacy owner with real temporary State and hostile files.
2. Instrument transport, framing, completed-response decoding and local acceptance failures with closed metadata and stack call sites. Enable after CLI admission without changing canonical Events or exact channels.
3. Seed Composer from setup, reuse same-source cached models, and run a picker-owned refresh. Extend existing cache-invalidation, frame and native picker owners for refreshing/failure/updated states, stable selection/draft and cancellation.
4. Clarify preparing versus waiting activity; validate small/colorless/linear frames and native Linux terminal restoration with synthetic fixtures.
5. Run repository formatting, strict lint, offline tests and executable build; compare against the pre-change snapshot. Update authoritative instructions, architecture and user docs. Leave actual account retry, macOS and assistive-technology checks on the final user checklist.

## Exit criteria

Safe stage-specific debug evidence survives a failure without changing its canonical disposition. Cached `/model` is immediately interactive during a bounded refresh, with visible progress/failure, unchanged authorization and retained draft/caret. Complete valid SSE is not rejected solely for its media label; malformed bodies and strict-outcome failures remain rejected. Local verification passes; the user's actual response body and live result remain explicitly unverified.

## Subscription compatibility follow-up

The user's development log records HTTP 200 rejected at `ContentType`, before any body bytes or events were read. This proves the rejection boundary, not the actual response format or a complete answer. The request already negotiates SSE with `Accept: text/event-stream`.

Use the one bounded SSE parser regardless of the response media label; do not add JSON/HTML fallback, response repair, retries or another route. Acceptance still requires a typed `response.completed`, the expected model, complete assistant message, strict requested outcome, valid positive usage within the local cap, and no selected-token reflection. Status, destination, encoding, byte and time guards remain unchanged. Native and catalog media admission is outside this change. Bump both ChatGPT diagnostic and account-admission contract versions because acceptance semantics change.

This is Arany's compatibility decision, not a claim that OpenAI guarantees missing or incorrect media labels. A mislabeled HTTP-200 error body may now be read before rejection, within the same byte/time bounds; neither that body nor its headers are logged. The actual live body remains unknown because the prior rejection happened before reading it.

Verification owners:

- Extend the existing successful streaming transport owner with normal, parameterized, absent and misleading media labels. Demonstrate rejection before the fix and acceptance afterward. Repeat existing semantic failures with absent/misleading labels so compatibility cannot bypass them.
- Strengthen the existing non-event transport owner with complete non-SSE JSON, HTML and plain text; retain compression and declared-overflow rejection.
- Extend the existing native synthetic ChatGPT journey so its first unchecked direct Run receives valid data-only SSE labeled as JSON. Preserve exact request channels, strict replay, failure diagnostics, two consecutive direct Runs and a team Run. Explicitly activate it in debug and optimized modes; do not claim live-account proof.
- Run formatting, strict all-target lint, both default offline suites and executable builds; review against the pre-follow-up snapshot. Update the narrow Provider rule and affected documentation. Real subscription retry stays on the user's final checklist, not a coding prerequisite.

## Change log

- `2026-10-04`: Created the focused implementation plan from the user's failure report and latency feedback.
- `2026-10-04`: Implemented bounded development diagnostics, setup catalog retention, cancellable asynchronous picker refresh and clearer waiting feedback. Updated authoritative rules, architecture and user operation without changing canonical acceptance or account authority. No commits or live account calls.
- `2026-10-04`: Followed the observed pre-body media rejection with a red/green transport regression and fixed-parser compatibility change. Removed the obsolete stage, bumped both ChatGPT contracts and extended the strict rejection corpus and isolated shipped-product journey. No request, endpoint, TLS, credential-storage, billing or canonical schema change.

## Verification and handoff

- Formatting and strict all-target Clippy passed in development and optimized modes. Both executable builds passed offline with the lockfile unchanged. The existing vendored Crossterm warning remains outside this change.
- Default offline suites passed: 297 tests with 56 ignored in development, and 284 tests with 60 ignored in optimized mode. Ignored gates are not claimed as passing by these counts.
- The explicitly activated native Linux synthetic ChatGPT journey passed once in each mode after the compatibility fix. Its first unchecked direct Run receives complete valid data-only SSE labeled as JSON; later direct and team Runs retain ordinary SSE. It asserts SSE request negotiation, holds catalog refresh until cached choices are visible, rejects a later refresh without losing them, and proves distinct EOF/incomplete diagnostics only in development with unchanged failed-Run replay and process channels. It uses an isolated account, private State and local HTTPS peer, not a real subscription.
- Before the fix, the exact streaming transport owner failed on a complete stream without a media label. Afterward all ten subscription owners passed, including valid normal/parameterized/absent/misleading labels, repeated usage/model/outcome/incomplete failures under different labels, and non-streamed completed JSON, HTML, plain text, encoding and declared-length rejection.
- Existing bounded-file and SSE owners cover private-file admission, retention, aliases, source-path exclusion, strict rejection and success. The catalog TestBackend corpus covers all refresh states at widths 16 through 120, with and without color, retained draft/caret and visible filter. The activated held-refresh journey uses the canonical linear terminal; native inline refresh and assistive-technology evaluation remain manual checks, not inferred from buffer rendering.
- Reviewed the source/test/documentation changes against the pre-task and pre-follow-up snapshots, including authorization, cancellation, output privacy and ownership. No dependency, migration, endpoint, TLS, billing or canonical Event changes were introduced.
- Live account retry, native macOS and assistive-technology evaluation remain on the final user checklist and do not block implementation. The observed pre-body rejection and local compatibility proof do not establish the actual response body or a repaired live account. This uncommitted plan remains available for review before an authorized landing.

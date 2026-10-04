# Beta implementation review cleanup

## Metadata

- Status: `Done`
- Owner: Arany maintainers
- Last updated: `2026-10-04`

## Goal and scope

Correct actionable findings in the current uncommitted beta implementation and repeat independent Standards and Spec reviews until neither reports an unresolved finding in that scope. The reference at review start was HEAD `c644fcef401f1657f0b3699e21a35b0216f2833a`, including the then-untracked `src/`, `tests/` and `scripts/` trees; a pre-goal filesystem snapshot pins that implementation independently of later Git tracking changes. Preserve existing user changes; make no commits, live account calls, or paid checks. User-owned account, macOS and assistive-technology checks remain the final handoff, not implementation blockers.

## Current state and ownership

The latest closed journal receipt records a ChatGPT `response_contract` failure after a terminal SSE completion. It does not identify its specific payload defect. The shared Responses decoder now distinguishes commentary from one final outcome, joins ordered final text parts and rejects contradictory completion markers. Subscription diagnostics preserve measured counters and separate final-decoding causes from structured-outcome failures. Synthetic corpus and product owners cover these repairs without claiming to reconstruct the user's private response.

Review also corrected pre-persistence MCP argument admission/credential scanning, minimum-budget Guard uncertainty receipts, refusal-fixture masking, unreadable count/depth forks and slash-control trailing whitespace. Each uses its existing authoritative owner and regression corpus.

Provider owns wire decoding, bounds and canonical error classification; diagnostics owns compiled local metadata; Store owns private log admission. Engine, Session and Tools retain their current boundaries. No new substitutable interface, crate, dependency, billing route, Event schema, endpoint or TLS change is planned. See [architecture](../../docs/architecture/system-overview.md), [Provider](../../agents/provider.md), [security](../../agents/security.md) and [testing](../../agents/testing.md).

## Chosen direction and security model

Fix each demonstrated defect at its existing owner. For Responses, distinguish explicit commentary from one final structured outcome while retaining legacy unphased single-message support. Validate every content item; do not accept malformed/refusal blocks, ambiguous final messages, unconstrained text, model drift or partial outcomes. Keep the existing request, response, usage, time and aggregate bounds. Diagnostics carry only closed categories and measured numeric metadata, never bodies, tokens, identifiers or conversation data. Acceptance changes invalidate every affected optional checker and ChatGPT admission contract.

## Execution and verification map

1. Independent Standards review covers Engine/Store/Session/Tools/Guard/MCP; Spec review covers chat/setup/catalog/commands; a separate Provider pass covers published protocol compatibility. Report only actionable evidence-backed defects, not taste or an absolute absence-of-bugs claim.
2. Extend the existing wire and transport corpora with a red-capable valid phase/multi-block scenario and corresponding hostile cases; correct the shared decoder and diagnostic propagation, then exercise Run and compaction through the synthetic product owner.
3. Reproduce each additional finding at its current owning corpus or integration seam, fix the cause, and have its reviewer recheck it. Keep cancellation, durable receipt and credential-isolation claims intact.
4. Consolidate formatting, strict all-target Clippy, default offline tests and executable builds in development and optimized modes. Run applicable activated local gates without live credentials. Review the complete change against a pre-goal source snapshot.
5. Update authoritative behavior documents and the final verification checklist. Repeat both independent review axes after the last correction. Retain this uncommitted plan through handoff because no landing is authorized.

## Exit criteria

No unresolved actionable Standards or Spec findings remain in the examined beta scope; all corrected defects have red-capable regression evidence and applicable local checks pass. Security acceptance and explicit resource bounds remain intact. Manual/live/platform evidence is accurately deferred, not presented as passing or as proof that the user's exact private response was repaired.

## Correction evidence

| Repair | Existing owner and observed evidence |
| --- | --- |
| Responses commentary, final text parts and contradictory completion markers | Shared inbound-wire corpus covers native/streamed Run and compaction, valid exact concatenation and hostile cases. The valid phased case failed before repair and passed afterward. |
| Completed-stream diagnostic causes and counters | Isolated ChatGPT checked-turn product journey passes in development and release: commentary stays out of output/history; model/outcome rejection retains measured bytes, event count, HTTP status and canonical reason; release creates no diagnostic log. |
| MCP argument admission and selected-credential reflection | Both native decoded-reflection owners failed on the overwritten escaped-value case before repair and pass with duplicate, malformed, encoded-key/value and non-object rows. Provider and Guard share the existing bounded strict parser. |
| Minimum-budget Guard uncertainty | Existing Tool admission corpus checks every error branch at the legal 128-byte result budget, retaining uncertainty and compiled labels. |
| Resulting fork lineage quota | Existing persistence-capacity integration owner demonstrated count and depth failures independently before repair; admitted boundaries, rollback, no Provider call and public closed replay now pass. |
| Refusal-test masking | Native MCP refusal corpus passes with otherwise-valid peers. A temporary duplicate-key parser mutant failed the intended rejection assertion; production parsing was restored and reverified. |
| Slash-command trailing whitespace | Existing registry corpus failed before repair and passes afterward, retaining exact literal/escaped objectives and rejecting embedded separators or extra arguments. |

The independent final Standards and Spec passes report zero unresolved actionable findings in their examined scope. A separate Provider/security pass also reports none, including an independent check of other authors' repairs and Workspace, clipboard and telemetry boundaries. This is scoped review evidence, not proof that all possible bugs are absent.

## Consolidated local verification

- Formatting and whitespace review pass; strict all-target Clippy and default locked offline workspace tests pass in development and release.
- Locked offline development and optimized executable builds pass; the local runnable outputs are `target/debug/arany` and `target/release/arany`. This is not redistribution or cross-platform approval.
- The optimized, explicitly activated `tools::` lane completes all eight native synthetic owners: coding/Skills/MCP replay, hostile MCP, budgets/topology, denied paths/receipts, descendant cancellation, network/credential isolation, kernel resource/OOM cleanup and shipped native-adapter `exec --tools`.
- The optimized `platform_tls_chain_and_name` owner completes its one protected child, accepting the trusted matching chain and rejecting untrusted chains and wrong names. Native macOS remains unperformed.
- The isolated ChatGPT checked-turn owner completes exactly once in each build mode; all endpoints, accounts and tokens are test-owned and synthetic. No live service, user's keyring or account is exercised.
- `cargo deny --locked --offline check` passes advisory, bans, license and source policy with its existing duplicate-dependency warnings. `cargo audit --no-fetch --no-yanked --format json` finds no vulnerability in the cached database; no fresh-fetch or yanked-crate result is claimed.
- No dependency, CI, public interface or Event schema changed. Optional checker/admission versions invalidate earlier contracts without adding native/ChatGPT preliminary inference or changing storage consent. Previously accepted invalid MCP arguments now fail strict replay; valid encoding is unchanged and no user data is migrated.
- The pre-existing third-party Crossterm parentheses warning remains outside this repair; its temporary EOF patch is preserved.

User-owned live account, native keyring, desktop clipboard, assistive-technology, macOS, minimum-toolchain and broader release checks remain in [next steps](../../NEXT_STEPS.md). They are not passing evidence and do not block this implementation-review handoff.

## Change log

- `2026-10-04`: Started the user-requested repeat-review goal with independent reviewers and a pre-goal source snapshot.
- `2026-10-04`: Corrected seven demonstrated defect/evidence areas and stale documentation; final independent Standards, Spec and Provider/security passes report no unresolved actionable findings. Consolidated local development/release tests, lint, executable builds and activated synthetic Linux lanes pass. Manual/live/platform and broader release checks remain explicitly user-owned; no commits were made by this workflow.

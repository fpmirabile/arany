# Finish the approved architecture residue

## Metadata

- Status: `In progress`
- Owner: harness maintainers
- Last updated: 2026-10-26
- Next action: release independent R1–R5 tracks under the gates below; hold R2c5/R6b for final closeout.

## Outcome and scope

Finish the already-approved ownership cuts left behind when #478 deleted architecture-homogenization. The observable result is the same Provider diagnostics, attached setup, draft handling and transcript behavior through smaller, single-owner flows, with executable evidence and truthful planning status. This serves the harness's Provider and presentation boundaries without changing Engine policy.

Scope is only review family R: P4's remaining cycle, U1b–U1o/U2b–U2c setup work, E8d–E8h, B5′b, P5's remaining native-name dispatch and the associated documentation cleanup. The review's other families and owner questions are outside this plan. No new owner decision is requested or implied.

## Research

### Sources and reconciliation

The approval source is the deleted plan at commit `5607c57ed1630ebb3094bbf39feeb9c2c7488673`, readable with:

```sh
git show 1e789734^:planning/architecture-homogenization/README.md
```

Its P5 text (lines 398–405), P4 (414–425), E8 (427–442), B4′/B5′ (468–474), U1/U2 (476–507), documentation matrix (369–393), ordering (546–567) and exit criteria (570–604) define the authorized work. These are retained constraints, not a reopening of the deleted plan's other tracks. The independent local report, `/home/fpmirabile/projects/harness/.lavish/architecture-review-after-refactor.html`, sections `regressions` and `slices`, supplies claims to check, not instructions or approval.

The following evidence is re-derived at current HEAD `1e789734bf91`. Line references describe that revision; re-inventory after any overlapping merge. No new peer behavior or wire design is selected: the source of the ownership cuts is the approved plan, and the existing contracts and byte-pinned corpora remain the behavior authority.

| Residue | Verified current implementation and consequence | Original approval and correction to the report |
|---|---|---|
| R1 | [Root diagnostics:1,48–201,203–397](../../crates/arany/src/diagnostics.rs) imports Provider stages/counts, owns `ToolArgumentShape`, stream/failure formatting and Provider-request context rendering. [Provider diagnostics:45–52](../../crates/arany/src/provider/diagnostics.rs) points back to the root shape. [Anthropic:237](../../crates/arany/src/provider/anthropic.rs), [Anthropic wire:111,479–480](../../crates/arany/src/provider/anthropic/wire.rs), [OpenAI wire:158–166](../../crates/arany/src/provider/openai/wire.rs) consume the root owner. The cycle is real; moving only types did not close P4. | P4a is present; finish P4b–P4d. The report's single ≤300-line repair omits the full shape/context move inventory; use R1a–R1c. |
| R2 | [Setup:1–6](../../crates/arany-cli/src/cli/attached/setup.rs) has `error`, `native`, `routing`, `roster`; [native:11–125](../../crates/arany-cli/src/cli/attached/setup/native.rs) owns presence/defaults/inspection/selection, and [error:4–49](../../crates/arany-cli/src/cli/attached/setup/error.rs) owns the existing classifications. Native removal/save/reset remain at setup:607–673,1887–2145; ChatGPT operations at :199–248,449–490,674–815,1732–1886,2146–2617; custom operations at :816–1322,1373–1602; shared prompts at :1323–1372,1603–1731,2618–2839,2853–3224. | U1a and the shared error owner/generic conversion sweep landed (#461/#463). Remaining U2b/U2c work is operation-specific mapping ownership, not another sweep of already-centralized `recoverable` calls; U1b–U1o still need their flow owners. Three aggregate moves cannot fit the original ceiling. Retain native and route owners as originally approved; secrets go to `native.rs`, not a new generic secret owner. Existing `routing.rs`/`roster.rs` stay intact. |
| R3 | [RunProgress:226–241](../../crates/arany-cli/src/cli/attached/run.rs) still stores `offered`, `linear_offers`, `held` separately. [Input:28–45,69–100,392–442](../../crates/arany-cli/src/cli/attached/run/input.rs) mutates them. Inline focused modules are budget :861–984 (124 lines), escape :2428–2480 (53), approval reply :2483–2526 (44), driver :2529–2761 (233). | E8d–E8h remain. There are **four**, not five, remaining focused modules and no separate inline burst module. Their 454 module lines alone cost 908 changed lines to move. Preserve the separate process/macOS `run::tests` at :988–2425 and its self-spawned names. |
| R4 | [Terminal:447–459,751–762,1459–1505](../../crates/arany-cli/src/terminal.rs) has `Panels` and `OwnedTerminal`, but history/classic-history/native/open/dump/adjacency/kept-tools are still separate `AttachedTerminal` fields. | Only B5′b remains; no repeat of the lifecycle or Panels cuts. [iTerm2 selection:687](../../crates/arany-cli/src/terminal.rs) and the existing graphics implementation establish that image-hover S2's implementation gate is cleared, despite its stale plan. |
| R5 | [Catalog:115–165,219,249](../../crates/arany/src/provider/catalog.rs) selects native endpoints, headers and paging with strings; [native check:130–150,482–524](../../crates/arany/src/provider/native_check.rs) selects adapters and fingerprint protocol text the same way; [credentials:442–483](../../crates/arany/src/provider.rs) repeats native names/environment selection. | P5's exit condition requires native-name policy through `ProviderKind`; these remaining sites violate that condition even though its initial file inventory omitted catalog/check. Keep external strings and fingerprint bytes unchanged. |
| R6 | [Index:52–56](../README.md), [image plan](../image-hover-preview/README.md), [limits plan](../spec-limits-generation/README.md) and the [context index/plan pair](../context-budgets/README.md) disagree about completed work. [NEXT_STEPS:24](../../NEXT_STEPS.md) still presents image S1/S2 as pending; the [overview:267](../../docs/architecture/system-overview.md) still describes singular `Tool`. [Compaction:299](../../crates/arany/src/engine/compaction.rs) still calls `HookRefusal::into_engine_error`, with no corresponding deferred item. | The original closeout explicitly required carrying that compaction-edge difference to NEXT_STEPS. R6 records it without changing its behavior. The Chat Completions gate follows O7 below; its remaining label/assertion stay in that feature plan. |

### Contracts and evidence owners

This is behavior-preserving work. No Event/record version, migration floor, CLI syntax, schema, wire payload, retry policy or authority changes. The same-PR documentation requirement from the original approval still applies, even to moves; update ownership/scenario links narrowly instead of creating new contract sections.

| Track | Preserved success and meaningful failure/recovery observations | Evidence owner |
|---|---|---|
| R1 | Same closed stages/counts and record text; debug-only activation, private bounded append; contention, unsafe files and storage failure discard logging without changing the Run. Context and Tool diagnostics disclose numeric shape, never values. | [Diagnostics acceptance](../../docs/specs/diagnostics.md#acceptance-and-evidence-owners); existing root record/privacy/file tests (root :609–678), [Anthropic wire tests](../../crates/arany/src/provider/anthropic/wire/tests.rs), [OpenAI wire tests](../../crates/arany/src/provider/openai/wire/tests.rs), [subscription tests](../../crates/arany/src/provider/openai/subscription/tests.rs), [custom check](../../crates/arany/src/provider/custom/check.rs), [Engine routing](../../crates/arany/tests/engine_run/routing.rs). Move producer-record tests with their owner; retain sink/file tests at root. |
| R2 | Admission before selected secret access; unchanged account/route choice, defaults, confirmation → journal start → publication → settlement order. Cancel preserves pre-save state; uncertain publication stops without retry; shutdown/presentation failures retain their classification and exact feedback. | [Terminal setup](../../docs/specs/terminal.md#setup-and-account-choices), [selected authorization](../../docs/specs/credentials.md#selected-item-authorization), [ChatGPT accounts](../../docs/specs/chatgpt.md#accounts-consent-and-storage), [Provider custom routing](../../docs/specs/provider.md#custom-destinations-and-routing); setup unit tables, [removal](../../crates/arany-cli/tests/session_run/setup/removal.rs), `session_run::{setup,custom,model_catalog,account_lock,roster}`, setup routing/roster corpora, [ChatGPT account tests](../../crates/arany-cli/src/accounts/chatgpt/account/tests.rs). |
| R3 | Same Goal budget refusal, escape/approval choice, current-Run-only exit settlement, delivered-note clearing and held-draft return order; overflow retains the newer composer and old draft recall; cancellation never loses a retained draft silently. | Existing relocated Run tests; [draft contract](../../docs/specs/terminal.md#draft-editing-and-submission); `session_run::{active_terminal,tools,rewind}`, especially [steering](../../crates/arany-cli/tests/session_run/active_terminal/steering.rs). |
| R4 | Same Fullscreen/Classic/linear history, pager/dump/reacquisition and kept-Tool suppression; partial acquisition, reset errors and failed reacquisition retain first-error cleanup and a released terminal. | [Terminal continuity/restoration](../../docs/specs/terminal.md#interruption-suspension-and-restoration); terminal unit tests :4349–4654,4746,4868,5100; [history](../../crates/arany-cli/src/terminal/history.rs), [frame corpus](../../crates/arany-cli/src/terminal/view/tests.rs), `session_run::active_terminal::{acquisition,broken_stderr,keyboard,panic}`, `inline_in_flight_suspend_releases_and_reacquires_raw_mode`, [chat mouse/dump](../../crates/arany-cli/tests/session_run/chat_mouse.rs), [macOS terminal](../../crates/arany-cli/tests/session_run/active_terminal_macos.rs). |
| R5 | Same selected native endpoint/header/env, account binding and catalog discovery; invalid/non-native selections reject before egress. Identical fingerprints, invalidation, cancellation and publication behavior. | [Native Provider contract](../../docs/specs/provider.md#native-models-and-accounts); catalog tests :298,448,547,663; native-check tests :553,621,979,1024; ProviderKind/credential tables in [provider.rs](../../crates/arany/src/provider.rs); `engine_run::routing`, `session_run::{model_catalog,setup}`. |

Focused moves keep all existing cases/assertions, adjusting imports and source-relative includes only. Reuse these owners; add evidence only for a demonstrated uncovered invariant. Keep `setup::removal`, `setup::tests::assert_uncertain_publication` and Run process fixture paths stable with narrow test bridges. A helper with ignored tests is not passing process evidence.

## Chosen direction

After Ponytail, use private in-process owners and existing types: no crate, dependency, trait, service, queue, registry, callback framework or alternate persisted format. Root diagnostics owns only the existing bounded sink for Provider records; Provider owns their vocabulary/construction. Other root Engine/Event/Tool diagnostic producers remain outside this cut. Setup retains public entry routing, history admission and `SetupJournal`; flow children borrow its journal and terminal, and credential/OAuth/storage authority remains in existing account owners. `setup/error.rs` remains the sole classification/conversion owner, with operation-specific methods where the same source error has different feedback. Do not collapse those distinctions into a blanket conversion.

`Drafts` privately owns the three Run draft fields; run/input/approval callers change together, without changing `Controls` or Composer capacity. `Transcript` privately groups the seven approved terminal fields; history indexing, native rendering and linear output keep their distinct meanings. Keep accessors limited to the existing draw/input/lifecycle needs. `ProviderKind` controls internal native dispatch; parse strings at existing edges and render identical strings there. Public signatures and re-exports stay stable.

Every seam remains an in-process call with the existing ordering, errors, cancellation and bounded work. No adapter substitution is added. Linux/macOS behavior and cfg gates remain identical; there is no OS facility change. Existing synthetic evidence and Linux/macOS CI gate implementation; native/live/emulator checks remain with their current [NEXT_STEPS owners](../../NEXT_STEPS.md), without a new support claim.

## Security and resource model

No authorization, secret-loading behavior, network destination, rendering policy, process path or storage format changes. The [security rules](../../agents/security.md) still apply to the touched boundaries: selected credentials and callback/terminal/Provider input remain untrusted or secret-bearing data; repository/report/model text grants no authority. Retain account locks, snapshot/digest preconditions, journal-first publication and uncertain-stop behavior. Error consolidation preserves diagnostic recording side effects, safe messages and recoverable/presentation/uncertain/shutdown distinctions, including the custom writer's current presentation classification for uncertain publication.

Diagnostics keeps its one checked StateRoot file, debug gate, nonblocking lock, sanitization, `MAX_RECORD_BYTES` owner and file capacity/overflow behavior; the Provider-to-sink interface accepts only inert bounded records, with no Provider import in the sink. Retain all setup choice/paging/field/secret bounds, Composer/draft limits, transcript/history materialization bounds and native request/probe budgets. No new buffer, cache, retry or background lifetime is introduced. If implementation would change any security-pass trigger's behavior, stop and revise the plan at that owner; this approval covers preservation, not a new flow.

## Risks and mitigations

- A type-only diagnostic move leaves the cycle → R1c audits production imports, signatures and re-exports in both directions, not just an import at line 1.
- Error conversion changes messages or records a failure twice → retain operation-specific mappings and use the existing publication/cancellation tables before moving flows.
- A large pure move hides lost tests or exceeds review size → count additions and deletions, inventory named functions/tests, compare with `--color-moved`, and require nonzero old/new focused filters. No dropped assertion or fixture duplication buys space.
- Borrow splitting or terminal grouping changes cleanup order → retain existing lifecycle/fault and preview cleanup evidence; avoid expanding into lifecycle redesign or frame restructuring.
- Overlapping feature work invalidates line budgets → use the gates below, re-inventory after integration and revise the bounded breakdown before implementation if it no longer fits.

## Vertical slices

Each implementation row is one coherent PR preserving its complete callable flow. R6b is the closeout of R2c5 in that same final PR, not an additional documentation-only PR. All numbers are estimated **added plus deleted lines**, including tests, imports, visibility, call-site glue, required spec/overview edits and plan maintenance; each is a hard ceiling. Moves count twice; `--color-moved` is review evidence, not an exemption. For move rows, `M/G` means at most M relocated production/test lines counted twice plus G other changed lines. Budgets reserve 80 glue/documentation lines for setup moves; other rows state their own allowance. Re-inventory before opening; split by operation/risk if a budget is exceeded, never defer its docs or failures. No original approval granted these residue moves an exception to 600.

### R6a — Reconcile status first

| ID | Complete increment and evidence | Estimate/ceiling |
|---|---|---|
| R6a | Refresh the four stale index rows and their plans: image S1/S2 implemented, only S3 pending its existing dependency approval; multi-call S1–S3 implemented, S4 still single-call on OpenAI at [wire:398](../../crates/arany/src/provider/openai/wire.rs); limits S1/S2 exported in [limits.rs:56–204](../../crates/arany/src/limits.rs), S3/S4 remain; context's current S3a–S3d/report breakdown is already maintained, so fix its stale index without reverting it. Reconcile NEXT_STEPS daily-use item 7 and the corresponding priorities. Carry the compaction `HookRefusal::into_engine_error` difference once under Tool/Engine follow-ups, linking [compaction:299](../../crates/arany/src/engine/compaction.rs), [stop mapping:126](../../crates/arany/src/engine/run_loop/stop.rs), [Engine hook replay evidence](../../crates/arany/tests/engine_run/hooks.rs) and [Guard pre-compact scenarios:5414–5640](../../crates/arany-cli/tests/guard_runs/permissions.rs), without changing it. Correct overview §5's singular Tool outcome to the existing bounded `Tools` list. Replace any remaining dead gates with this plan's IDs, preserving genuinely open feature gates. Evidence: source inventory, local links/anchors and spelling; implemented is not a claim that checks ran. | ≤120 |

### R1 — Finish P4's one-way diagnostic boundary

Serial R1a → R1b → R1c. Original P4a types already exist; all three PRs include `docs/specs/{diagnostics,provider}.md` and overview §2/§3, stating the exact ownership reached in that PR and preserved failure/privacy evidence.

| ID / original | Complete increment | Estimate/ceiling |
|---|---|---|
| R1a / P4b | Move `ToolArgumentShape`, Debug formatting and impl (root :48–201) to `provider/diagnostics.rs`; adapt `StreamCounts` and Anthropic/OpenAI wire consumers/tests, removing root shape aliases. | ≤420 (≤160/100) |
| R1b / P4c | Move stream-limit and subscription/Messages failure entry points/record formatting (root :203–273) and their focused record tests (:609–637) to Provider. Expose the existing bounded inert-record sink narrowly; keep activation/root/lock/file admission/caps at root, and adapt all streaming/subscription/custom/Anthropic producers. | ≤450 (110/230, including producer glue) |
| R1c / P4d | Move `tool_context_full`/`context_record` (root :274–397) and context privacy test (:639–676), adapt its Engine caller, then prove no production root diagnostics → Provider dependency or Provider re-export remains. Provider → root sink remains permitted. | ≤500 (170/160, including callers/audit docs) |

Use the R1 evidence row for every increment's affected producers; R1c closes only with both direction scans and unchanged failure-record/privacy/bounded-file evidence. Root may still depend on Engine/Event/Tools for its unrelated producers.

### R2 — Finish U1/U2 with stable setup entry points

R2p1 → R2p2 precede every remaining flow move, as the original U2-before-U1 sequencing required. Then execute R2p3 → R2p4 → R2p5 → R2a1–R2a4 → R2b1–R2b4 → R2c1–R2c5 serially: one setup entry-point/test owner at a time. Original U1a/U2a, roster/routing feature setup and Provider prefix construction are already present; do not redo them.

Each inventory below refers to current `cli/attached/setup.rs`. Private sibling calls may temporarily use narrow imports from setup until their own operation moves; remove superseded helpers/re-exports as the last consumer transfers. No wrapper-only module, duplicate error enum or copied process fixture.

| ID / original | Complete operation and destination; focused evidence travels with its flow | Estimate/ceiling |
|---|---|---|
| R2p1 / U2b | Finish native operation-specific conversion ownership: move `native_removal_error` (:761–772) and native-save uncertainty message construction (:2045–2059) behind narrow `error.rs` methods; keep journal disposition/settlement in the flow. Audit saved-access/native calls already converted by #463 without rewriting them. Preserve exact text and diagnostic side effects. | ≤200 |
| R2p2 / U2c | Finish operation-specific mappings through `error.rs`: `chatgpt_removal_error` (:752), `custom_write_error` (:1098), authorization/uncertainty/keyring mappings (:2485–2516) and their focused fallback assertion. Audit existing routing/roster conversions from #463 without redoing them. Keep safe custom-check status vocabulary with its display owner; retain exact messages, logging and cancellation paths. | ≤300 |
| R2p3 / U1b | Move `remove_native_saved_access`, `settle_native_removal`, `reset_saved_setup` (:607–673,2064–2145) to `native.rs`; setup keeps mixed-route removal dispatch. | ≤420 (170/80) |
| R2p4 / U1c, save flow | Move `inspect_or_file`, `native_setup`, `settle_native_save` (:491–503,1887–2063) to `native.rs`; preserve lookup → choice → catalog → journal → save/settle. | ≤480 (200/80) |
| R2p5 / U1c, catalog/default flow | Move `native_catalog_default`, `load_catalog` (:2840–2852,3225–3241) and `setup_account_requires_explicit_effort_for_unreviewed_model` (:3430–3471) to `native.rs`. | ≤250 (85/80) |
| R2a1 / U1d | Move `ChatGptSetupAccount` and impl (:199–248), `chatgpt_setup`, action enum/action (:1732–1850), `choose_saved_chatgpt` (:2146–2216) to `chatgpt.rs`; preserve menu → selected action → result/default settlement. | ≤600 (260/80) |
| R2a2 / U1e | Move ChatGPT local removal, saved-ID choice/labels (:674–760,773–815), inspection (:1851–1886), selection (:2217–2257), selected/account defaults (:449–471), and colliding-ID label test (:3419–3427) to `chatgpt.rs`; mappings already transferred by R2p2 are excluded. | ≤570 (245/80) |
| R2a3 / U1f | Move `connect_chatgpt` through `save_chatgpt_authorization` (:2258–2484) and `await_chatgpt_step` (:2803–2814) to `chatgpt.rs`; retain authorization → publication → journal settlement, cancellation and uncertainty bridges. Process fixture bodies stay put. | ≤570 (245/80) |
| R2a4 / U1g | Move `configure_chatgpt`, `review_chatgpt_risk` (:2517–2617), `catalog_chatgpt_defaults` (:472–490), and matching visible-catalog/default test (:3483–3569) to `chatgpt.rs`. The keyring fallback test (:3572–3585) follows its R2p2 error classification owner, with its assertion preserved. | ≤520 (220/80) |
| R2b1 / U1h | Move `custom_routes_setup`, route result/context, selection/action and `select_custom_profile` (:816–968) to private `routes.rs`; do not merge existing role `routing.rs` into it. | ≤400 (160/80) |
| R2b2 / U1i | Move custom edit/remove/add (:969–1097), write/settlement (:1109–1144) and route-reference check (:1394–1402) to `custom.rs`, including the context they need from `routes.rs`; preserve reference refusal and snapshot-bound publication. | ≤460 (190/80) |
| R2b3 / U1j | Move profile choices/status/date, inspect/check/await/status (:1145–1322) and safe check-error display (:1373–1393) to `custom.rs`; retain inspection/check cancellation and safe status text. | ≤520 (220/80) |
| R2b4 / U1k | Move form/protocol/fields/effort parsing (:1403–1602) and `empty_efforts_create_and_clear_the_no_override_state` (:3473–3480) to `custom.rs`; preserve all three current protocol choices. | ≤530 (225/80) |
| R2c1 / U1l, warning/confirmation | Move `review_warning`, linear warning/confirmation (:2618–2802) to `prompts.rs`; preserve full review, paging, Back focus and resize/resume restart. | ≤460 (190/80) |
| R2c2 / U1l, acknowledgment/wait | Move `await_setup_ack`, `setup_message_pages` (:1323–1372), shared `await_operation` (:2815–2839), its page-width constant and paging test (:3407–3416) to `prompts.rs`; preserve cancellable wait and complete bounded content. | ≤270 (95/80) |
| R2c3 / U1m | Move `choose`, `choose_focused`, display/linear/inline choice and bounded input helpers (:2853–3051), with the choice-input limit, to `prompts.rs`; use existing linear-choice tables at their current owner, without duplicating them. | ≤500 (210/80) |
| R2c4 / U1n | Move `read_api_key`, hidden-key entry and workspace-ID entry (:3052–3224) to `native.rs`, reusing `prompts.rs`; retain secret echo release/restoration, paste validation and selected-key scope. | ≤440 (180/80) |
| R2c5 / U1o | Move required/optional shared field entry, `SetupValueInput` and inner validation loop (:1603–1731) to `prompts.rs`; preserve empty-keeps/empty-clears, bounds, cancellation and shutdown. Remove remaining superseded flow imports from setup. This is also R6b, held for final closeout after every other track. | ≤600: ≤360 (140/80) for the flow/docs, plus ≤240 for R6b |

Owner decision O7, 2026-10-26: Chat Completions S2 proceeds in the current `cli/attached/setup.rs`; R2b1–R2b4 carry its protocol inspection label and setup-journey assertion along when moving those flows.

Required same-PR docs, **plus overview §4 for every row**:

| Slices | Required specs in that PR | Narrow edit |
|---|---|---|
| R2p1, R2p3–R2p5, R2c4 | `docs/specs/{terminal,credentials}.md` | Native/selected-access/secret flow owner and preserved recovery/restoration evidence. |
| R2p2 | `docs/specs/{terminal,provider,chatgpt}.md` | Shared conversion owner and exact existing feedback/classifications. |
| R2a1–R2a4 | `docs/specs/{terminal,chatgpt,credentials}.md` | ChatGPT operation ownership and unchanged consent/publication/cancellation evidence. |
| R2b1–R2b4 | `docs/specs/{terminal,provider}.md` | Custom/route operation ownership, validation/check scenarios and evidence links. |
| R2c1–R2c3, R2c5 | `docs/specs/terminal.md` | Shared prompt ownership and unchanged paging/input/recovery observations. |

Every R2 PR runs the applicable setup/removal tables and the R2 process owners above. Specifically preserve `setup::tests::assert_uncertain_publication` (:3251–3326) and `authorization_exits_settle_and_reopen_without_pending_edits` (:3329–3403) at their stable bridge; they remain shared journal/process evidence, not a new generic test owner.

### R3 — E8d–E8h, Drafts and focused Run evidence

Serial R3a → R3b → R3c → R3d → R3e. Each PR includes `docs/specs/terminal.md` and overview §8: draft retention/overflow ownership or corrected focused/process evidence links, without new behavior.

| ID / original | Complete increment | Estimate/ceiling |
|---|---|---|
| R3a / E8d | Introduce private `Drafts { offered, linear_offers, held }`, adapting `run.rs`, `run/input.rs` and any approval caller together. Preserve return/steering/overflow/exit order and existing limits. | ≤300 |
| R3b / E8e | Move budget and escape modules (177 module lines plus cfg/spacing, ≤180) through new `run/tests.rs` or focused children; correct ancestor imports. Keep process `run::tests` unchanged. | ≤440 (180/80) |
| R3c / E8f remainder | Move approval-reply module (44 lines plus cfg/import glue) through that owner; no absent burst module is recreated. | ≤180 (50/80) |
| R3d / E8g | Move driver `end_of_input_waits_for_only_the_current_run_to_settle` and `exit_waits_for_only_the_current_run_to_settle` (:2536–2645, ≤120 including attributes) through the focused test owner. | ≤300 (120/60) |
| R3e / E8h | Move remaining driver steering/delivery/held/overflow/retention cases (:2648–2760, ≤130 with module glue), removing the superseded inline driver module. Old/new filters must execute nonzero matching cases; process/macOS filters stay unchanged. | ≤320 (130/60) |

Use the R3 evidence owners, compare test names/assertions before and after each move, and include existing Run input/approval tests. A lower file length alone does not close this track.

### R4 — B5′b Transcript

| ID / original | Complete increment and same-PR docs/evidence | Estimate/ceiling |
|---|---|---|
| R4 / B5′b | Group `history`, `classic_history`, `native`, `transcript_open`, `scrollback_dump`, `history_adjacent`, `kept_tools` in private `Transcript`; adapt construction, draw/input/dump, release/reacquire and released-state replacement as one usable flow. Keep linear state, signal/secret/clipboard ownership and graphics cleanup order intact. Evidence: the R4 unit/history/frame/PTY/fault owners above. Same PR: `docs/specs/terminal.md` continuity/pager/restoration ownership/evidence and overview §8's Panels/Transcript sentence. | ≤550 |

Image S2 and `OwnedTerminal`/Panels prerequisites are present. Reserve `terminal.rs` and shared PTY evidence against [image-hover S3](../image-hover-preview/README.md#vertical-slices); no concurrent terminal PR. No field-count target substitutes for behavior evidence.

### R5 — P5 native-name residue

| ID / original | Complete increment and same-PR docs/evidence | Estimate/ceiling |
|---|---|---|
| R5 / P5 completion | Use `ProviderKind` for the remaining catalog/check/`NativeApiCredentials` native classification and dispatch, parsing at public string boundaries. Preserve native/custom refusal, profile identity, headers/URLs/env names, telemetry labels and fingerprint bytes. No stored field conversion or API rename. Evidence: R5 owners, especially `probe_fingerprint_contract_preserves_persisted_bytes` and account/key/workspace binding tests. Same PR: original P5 matrix's `docs/specs/{provider,telemetry}.md` and overview Provider ownership; no telemetry wire change. | ≤300 |

### R6b — Final checked closeout in R2c5

| ID | Complete increment and evidence | Estimate/ceiling |
|---|---|---|
| R6b / R2c5, one PR | Hold the final U1o field-entry operation until all other tracks finish. In its working diff, satisfy every exit item below with PR/test/CI or audit evidence, then delete this plan and its index row and rewrite inbound residue gates to current contract/evidence owners. Keep unresolved verification and the compaction-edge difference in NEXT_STEPS. Its code, focused evidence, terminal spec and overview edits ship with deletion; no separate docs-only closure or deferred code documentation. Check local links, anchors, spelling and final diff. | ≤600 combined: ≤360 flow/docs + ≤240 closeout, including this 198-line README deletion and inbound-link edits |

## Gates and order against active plans

- **R6a status reconciliation is implemented; last R2c5/R6b in one PR.** Between them R1, R2, R3, R4 and R5 can progress independently only with exclusive production and evidence ownership. R1/R5 serialize `provider.rs` and shared Provider-spec edits. All shared spec/overview edits acquire a short integration reservation too.
- **OpenAI multi-call integration:** S4 is implemented under the [Provider contract](../../docs/specs/provider.md#structured-outcomes-and-tool-encoding). R1a–R1c and R5 must re-inventory the current wire, diagnostic, Provider registration and credential owners before overlapping edits, preserving the multi-call contract and snapshots. Do not reintroduce old single-call snapshots. No dead P4-complete claim satisfies R1c.
- **[Chat Completions family](../openai-chat-completions-family/README.md#gates):** S1 and S2's form/recipe exist. Under O7, the remaining S2 inspection label/journey proceeds now in `cli/attached/setup.rs`; it has no R2p2/R2b dependency. Reserve the current file and setup/custom evidence against overlapping moves, then carry the feature through R2b. The label implementation stays in its own feature PR.
- **[Image hover](../image-hover-preview/README.md):** S1/S2 implementation clears the old B5′b gate; R4 serializes with S3 as above. Pending S3 dependency approval is not imported into this plan or decided here.
- **[Spec limits](../spec-limits-generation/README.md):** its S1/S2 output exists. Its blocking S3 prose scan follows the remaining same-PR spec edits in R1–R5; coordinate S4's native Tools link moves without expanding this plan into Tools splitting or CI work.
- **[Context budgets](../context-budgets/README.md):** retain its live S3a–S3d and reporting sequence. It can proceed independently; reserve `tools.rs`/Run presentation or `terminal.rs` only if an actual changed file overlaps R3/R4. Its special compilation/check policy stays with that plan, not this track.
- **[Command overlay](../command-workspace-overlay/README.md), [shared editors](../shared-workspace-editors/README.md), [network proxy](../network-proxy/README.md):** no new implementation dependency on these residue cuts. Preserve their existing overlay/editor/egress ordering and reserve shared specs, overview or acquired evidence; do not revive historical B15/G5 gates as unfinished residue.

Each implementation PR follows [targeted verification](../../docs/development.md#verification): formatting, Clippy (including the production-only passes), its existing affected corpora, typos and diff hygiene; full required Linux/macOS CI must be green before merge. Build only in its worktree's default target or through `/home/fpmirabile/projects/harness-team/bin/cargo`, never set `CARGO_TARGET_DIR` or write build output in `/tmp`. This planning-only change runs no Cargo commands.

## Exit criteria

Leave items unchecked until their named evidence exists; an evidence owner or source observation is not a passing run. Put completed execution results in PR/CI history and cite them when checking the item, rather than accumulating a task log here.

- [ ] **R1a–R1c:** Provider vocabulary, shape, stream/failure and context construction have one Provider owner; production dependency/signature/re-export scans show root diagnostics → Provider = zero and Provider → bounded sink retained. Evidence: both scans plus relocated record/privacy tests, root bounded/private-file owner and affected wire/subscription/custom/routing results.
- [ ] **R2p1–R2p5:** native removal/save/default flows delegate to `native.rs`, one `error.rs` preserves all four classifications, operation-specific text and diagnostic side effects. Evidence: selected-access, native account/reset/removal, uncertainty/reopen and explicit-effort setup cases in the R2 owners.
- [ ] **R2a1–R2a4:** ChatGPT menu/removal/selection/auth/publication/configuration/defaults live in `chatgpt.rs`; existing account owners still control credentials and OAuth. Evidence: account uncertainty/selection/fallback tests, setup offline/process journeys, preserved `assert_uncertain_publication` bridge and no dropped focused assertions.
- [ ] **R2b1–R2b4:** custom routes/form/check flows live in `routes.rs`/`custom.rs`, separate from existing role routing/roster. Evidence: custom success/refusal/cancellation/publication journeys, all three protocol form paths, effort table, reference checks and unchanged saved-profile bytes. The O7 protocol label and journey assertion survive the moves; Chat Completions has no dependency on these slices.
- [ ] **R2c1–R2c5:** shared dialogs/choices/fields live in `prompts.rs`; secret/workspace entry stays native. Evidence: existing paging/linear-choice/input tables and setup/secret-echo/restoration journeys, with cfg gates, bounds, cancellation and exact text retained.
- [ ] **R3a–R3e:** `Drafts` owns offered/linear/held state and all four remaining focused modules are under `run/tests.rs`/children. Evidence: nonzero old/new filtered case counts and assertion inventories, draft/steering/overflow/current-Run exit results, unchanged process/macOS/self-spawn paths and the R3 journeys.
- [ ] **R4:** `Transcript` owns exactly the approved seven fields without merging distinct histories or linear behavior. Evidence: history/frame/Classic/pager/dump results and existing release/reset/partial-acquisition/reacquisition/preview cleanup fault owners; no field-count-only completion claim.
- [ ] **R5:** remaining native family policy uses `ProviderKind`; intentional serialized/protocol strings stay at their edge. Evidence: production-site scan, catalog/native-check/credential tables, identical fingerprint contract and account/workspace binding/reopen evidence; no migration or snapshot churn.
- [ ] **Same-PR documentation and size:** every implementation PR includes its matrix specs and overview edit within its row's ceiling. Evidence: full added/deleted diff stats including moves/tests/docs, `--color-moved` review for moves, current spec/evidence links and unchanged unrelated schema/snapshot/migration fixtures.
- [ ] **Verification and platform disposition:** targeted checks and required full Linux/macOS CI are green for every implementation head. Evidence: PR check results/run links; missing native/live/emulator evidence stays explicitly with its current NEXT_STEPS owner and creates no new support claim.
- [ ] **R6a and closeout readiness:** active plan/index pairs and NEXT_STEPS image status reflect the current remaining work, overview describes bounded `Tools`, dead dependency names are gone, and the compaction-edge difference has one NEXT_STEPS owner linked to its implementation/existing compaction-hook evidence. Evidence: source-to-plan audit, local link/anchor check, typos and diff hygiene; no other review family or owner decision entered the diff.

**The final slice R6b deletes this plan only after every checklist item is checked with its evidence.** It removes the index row and updates inbound links in the same PR under the [planning lifecycle](../README.md#lifecycle); no premature deletion based on merged slice names or file lengths.

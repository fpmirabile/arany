# Clean Architecture review

**Date:** 2026-10-04  
**Status:** scoped improvements implemented and locally verified  
**Scope:** three independent code reviews using Robert C. Martin's principles, followed by bounded improvements. This is not a full security audit, live compatibility proof or cross-platform certification.

## Review method

Three reviewers followed actual success, failure and cancellation paths with complementary ownership: dependency direction; responsibility/cohesion; and behavioral substitution/interface contracts. Root reconciled their evidence before assigning disjoint fixes. The [development skill](../../agents/skills/clean-architecture-review/SKILL.md) records the repeatable procedure and [Martin's primary sources](../../agents/skills/clean-architecture-review/SKILL.md#attribution); the findings below are project judgments. The [verification record](#verification) retains scoped evidence; current behavior lives in the [terminal](../specs/terminal.md) and [execution](../specs/execution.md) contracts.

## Confirmed strengths

- Engine consumes semantic Provider outcomes, not HTTP, terminal or SQL objects. Provider has actual native/custom/subscription and scripted implementations, earning the existing substitution seam.
- Store exchanges domain Events/views with one bounded connection-owning thread. SQLite transaction and acknowledgement mechanics remain behind its concrete interface.
- Effects retain one lifecycle owner: admission, committed proposal/intent, guarded dispatch, committed observation, then inference. Guard and credential processes have actual privilege and cleanup responsibilities, not diagram-driven layering.
- Child calls have isolated read-only context, bounded scheduling, deterministic result order and owned cancellation/draining. Presentation observes acknowledged facts and cannot grant authority.

## Accepted findings

| Finding | Principle and concrete consequence | Smallest repair | Status |
|---|---|---|---|
| Native OpenAI's standalone adapter ignores the caller's exact reported-output cap; custom transport omits the shared reported-input ceiling | LSP: public-library calls can publish successful responses outside the common reported-usage contract | Pass the exact cap into native decode admission and enforce the shared input ceiling in both transports; retain optional native and positive required custom usage | Applied |
| The public Provider trait omits important behavioral obligations | ISP/LSP: an implementer sees signatures but not stable resolution, disclosure, limits or cancellation semantics | Concise rustdoc on the existing interface, not another abstraction | Applied |
| Catalog loading uses ordinary rather than busy drawing | SRP: a query owns Enter but its composer advertises submission; canonical byte tracking and local progress/history can drift | Reuse busy drawing, restore retained bytes, finish accepted canonical segments before selection, and separate transient cues from errors | Applied |
| Linear help duplicates the compiled command registry in a truncated notice | SRP/OCP: aliases disappear and adding a command requires unrelated CLI copy updates | Route idle/active help through the existing terminal-owned registry projection with bounded append-only entries | Applied |

The adapter repair deliberately rejects over-cap native OpenAI responses before Engine outcome classification. It does not create a remote billing guarantee, retry, encoding fallback or billing-route change. Skill/install changes affect development agents only, never executable Tool grants or runtime command registration.

Native checker version v5 invalidates older optional diagnostics without making checks mandatory for ordinary native or ChatGPT Runs. Exact-custom checker fingerprints v4 replace v3: older saved custom evidence requires an explicit, potentially billable data-free recheck before new Workspace-bearing use. Nothing rechecks automatically.

### Evidence and change owners

- **P2, demonstrated, high confidence: adapter substitution.** Public `Provider::invoke`/`compact` reaches [OpenAI decode admission](../../crates/arany/src/provider/openai.rs#L211) or [custom transport usage admission](../../crates/arany/src/provider/custom/transport.rs#L153). The existing OpenAI decode/reflection corpus failed for a request cap of seven with eight reported output tokens; the existing custom loopback transport owner failed above the shared input ceiling. Both now accept the exact boundary and reject the excessive value for Run and compaction. These are adapter contracts, not remote billing proofs.
- **P3, justified structural improvement, high confidence: interface documentation.** The established multi-adapter [Provider trait](../../crates/arany/src/provider.rs#L498) now names immutable resolution, semantic outcomes, usage, cancellation and Engine-owned authority. No method signature, new trait or dispatch model was introduced. Existing adapter and Engine tests own its executable behavior.
- **P2, demonstrated, high confidence: catalog/input ownership.** CLI catalog loading reaches [the existing terminal busy draw](../../crates/arany-cli/src/cli/attached/models.rs#L121); completion consults [terminal-owned readiness](../../crates/arany-cli/src/terminal.rs#L429). The retained Composer remains the draft owner. Enter cannot submit while waiting, accepted canonical segments finish before selector ownership, and cancellation discards an undelivered accepted suffix. Linear selector suspension uses its own byte count. Existing reader, frame and model-catalog process owners cover these bounded claims; exact held-query completion timing remains unverified.
- **P2, demonstrated, high confidence: one help owner.** Idle and active CLI routing reaches `AttachedTerminal::open_help`, then [linear registry rendering](../../crates/arany-cli/src/terminal/linear.rs#L46). The existing screen-reader catalog journey failed waiting for `/exit` in the old truncated help. Complete exact linear output and public idle/active process journeys now cover all 18 entries, aliases, literal arguments, inert help and active cancellation. No modal focus or input prompt is acquired during active linear work.

Progress and retained-draft cues remain transient; actual errors remain chat feedback. Catalog readiness covers only an already accepted segment, not arbitrary queued physical lines or text still being edited by the OS. This is not a lossless typeahead-isolation guarantee. The final user-owned UX checklist records the residual timing and suspension checks without blocking implementation.

## Deferred and rejected recommendations

| Observation | Disposition and reason | Revisit trigger |
|---|---|---|
| Profile checks occur both before Workspace access and during strict replay | Retain both defensive boundaries; no current drift demonstrated | Actual inconsistent invariant or an existing cohesive value owner that removes maintenance risk without weakening either boundary |
| Session, Provider and Tool semantic types reference one another | Retain one-package domain references; they are not wire/driver leaks | Independently released adapters or measured dependency/ownership pressure; restructuring requires approval |
| Public Store errors expose the underlying SQLite cause | Retain useful causes; Engine does not branch on driver details | A real caller requires backend-neutral recovery or another Store implementation exists |
| Feedback lifetime is inferred partly from compiled notice strings | Fix demonstrated catalog/retained-draft drift; defer broader typed-notice interface work | Another observed lifecycle mismatch or an approved presentation-interface redesign |
| Traits for every Store, Guard, transport or Tool; crates per architecture circle; generic domain dump; runtime plugins | Reject speculative layering and authority expansion | Concrete substitution, isolation, release or ownership need, with the existing security admission requirements |
| Splitting files by length | Reject as an architectural finding | Independently changing responsibility or useful information-hiding boundary |

## Verification

Tests: [`crates/arany/src/provider/openai/wire/tests.rs::inbound_wire_corpus_requires_one_completed_structured_answer`](../../crates/arany/src/provider/openai/wire/tests.rs); [`crates/arany-cli/src/terminal/linear.rs::linear_presentation_is_labeled_append_only_and_control_free`](../../crates/arany-cli/src/terminal/linear.rs); [`crates/arany-cli/tests/session_run/model_catalog.rs::screen_reader_model_catalog_selects_exact_profile_without_provider_egress`](../../crates/arany-cli/tests/session_run/model_catalog.rs).

No real account, live Provider, native macOS or manual assistive-technology checks are claimed. No held compiled-native catalog fixture proved query completion precisely mid-segment or every suspension timing; reader and source evidence do not replace that integration evidence. Existing Guard, TLS, credential-service and performance lanes were not reactivated for this review because their implementation boundaries did not change. Ignored tests are not passing evidence.

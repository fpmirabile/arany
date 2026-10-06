# Clean Architecture review

**Date:** 2026-10-04  
**Status:** scoped improvements implemented and locally verified  
**Scope:** three independent code reviews using Robert C. Martin's principles, followed by bounded improvements. This is not a full security audit, live compatibility proof or cross-platform certification.

## Review method

Three reviewers followed actual success, failure and cancellation paths with complementary ownership: dependency direction; responsibility/cohesion; and behavioral substitution/interface contracts. Root reconciled their evidence before assigning disjoint fixes. The [development skill](../../agents/skills/clean-architecture-review/SKILL.md) records the repeatable procedure; [primary-source research](../research/clean-architecture-principles-2026-10.md) separates Martin's principles from project judgments. The [verification record](#verification) below retains scoped evidence; current behavior lives in the [terminal](../specs/terminal.md) and [execution](../specs/execution.md) contracts.

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

- **P2, demonstrated, high confidence: adapter substitution.** Public `Provider::invoke`/`compact` reaches [OpenAI decode admission](../../src/provider/openai.rs#L211) or [custom transport usage admission](../../src/provider/custom/transport.rs#L153). The existing OpenAI decode/reflection corpus failed for a request cap of seven with eight reported output tokens; the existing custom loopback transport owner failed above the shared input ceiling. Both now accept the exact boundary and reject the excessive value for Run and compaction. These are adapter contracts, not remote billing proofs.
- **P3, justified structural improvement, high confidence: interface documentation.** The established multi-adapter [Provider trait](../../src/provider.rs#L498) now names immutable resolution, semantic outcomes, usage, cancellation and Engine-owned authority. No method signature, new trait or dispatch model was introduced. Existing adapter and Engine tests own its executable behavior.
- **P2, demonstrated, high confidence: catalog/input ownership.** CLI catalog loading reaches [the existing terminal busy draw](../../src/cli/attached/models.rs#L121); completion consults [terminal-owned readiness](../../src/terminal.rs#L429). The retained Composer remains the draft owner. Enter cannot submit while waiting, accepted canonical segments finish before selector ownership, and cancellation discards an undelivered accepted suffix. Linear selector suspension uses its own byte count. Existing reader, frame and model-catalog process owners cover these bounded claims; exact held-query completion timing remains unverified.
- **P2, demonstrated, high confidence: one help owner.** Idle and active CLI routing reaches `AttachedTerminal::open_help`, then [linear registry rendering](../../src/terminal/linear.rs#L46). The existing screen-reader catalog journey failed waiting for `/exit` in the old truncated help. Complete exact linear output and public idle/active process journeys now cover all 18 entries, aliases, literal arguments, inert help and active cancellation. No modal focus or input prompt is acquired during active linear work.

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

| Lane | Current result and owned claim |
|---|---|
| Skill validation and portability | Frontmatter valid; pinned offline local install, byte-identical installed source and relocated local-manifest restoration passed. All three reviewers used the skill without material instruction ambiguity. |
| Adapter regression owners | Existing OpenAI corpus and custom loopback transport owner demonstrated RED before the repairs and GREEN afterward for Run/compaction boundary rows; all 37 Provider tests passed. |
| Terminal regression owners | Exact append-only/control-free linear output, accepted canonical-segment delivery/cancellation, three model-catalog process owners and held-Provider active-help cancellation passed. Active help covers inline and linear routing without a second input prompt or lost cancellation ownership. |
| Consolidated default debug | `cargo test --quiet --locked --offline`: 296 passed, 53 ignored, zero failures. |
| Consolidated default release | `cargo test --quiet --release --locked --offline`: 283 passed, 60 ignored, zero failures. |
| Formatting and lint | `cargo fmt --all -- --check` and strict all-target Clippy passed in debug and release. The unchanged vendored Crossterm dependency still emits its pre-existing parentheses warning. |
| Executable | Locked offline optimized build and the shipped binary's data-free `--help` passed. |
| Final review | Independent review accepted the production fixes; root reviewed direct source/test/documentation diffs against pre-edit snapshots, skill provenance, security bounds and deferred claims. No commits, dependency or CI changes. |

The first consolidated debug/release runs found an additional stale compact-help readiness marker in the existing rejected-command PTY journey. Its exact owner reproduced the failure. The gate now waits for the fresh complete `/exit` row and subsequent `Input:` while preserving rejection separation, terminal restoration and closed Run-free replay. Consolidation above passed after that change; completeness stays owned by the existing linear/catalog owners rather than duplicated in that journey.

No real account, live Provider, native macOS or manual assistive-technology checks are claimed. No held compiled-native catalog fixture proved query completion precisely mid-segment or every suspension timing; reader and source evidence do not replace that integration evidence. Existing Guard, TLS, credential-service and performance lanes were not reactivated for this review because their implementation boundaries did not change. Ignored tests are not passing evidence.

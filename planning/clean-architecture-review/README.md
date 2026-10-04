# Clean Architecture review and bounded improvements

## Metadata

- Status: `Done — scoped local review and implementation`
- Owner: Root development agent and three architecture reviewers
- Last updated: `2026-10-04`

## Goal

Review Arany independently with three complementary Robert C. Martin lenses, create a reusable project development skill, and apply confirmed, locally verifiable improvements without disrupting the functional beta foundation.

## Scope

- In scope: dependency direction, cohesive ownership, behavioral contracts, development skill source/installation, reconciled findings and justified bounded fixes.
- Out of scope: commits, live Provider calls, real credentials or wallets, dependency upgrades, CI changes, speculative framework/crate rewrites, or reopening deferred beta checks.

## Current state and ownership

The [canonical architecture](../../docs/architecture/system-overview.md) defines one package, a deep Engine, a concrete SQLite owner, multiple Provider adapters and separately supervised privilege boundaries. The [skill workflow](../../agents/skills/README.md) owns editable skill sources and generated installation hashes. Review findings must cite current code, not merely this declared design.

| Responsibility | Owner | Boundary |
|---|---|---|
| Stable Run coordination and receipts | Engine / Session | Semantic in-process values |
| Durable state and capacity | Store | Dedicated SQLite owner thread |
| Model behavior variation | Provider | Established semantic adapter contract |
| Effects and enforcement | Tools / Guard | Typed intent and privilege boundary |
| Chat / terminal interaction | CLI / Presentation / Terminal | Domain projection and terminal ownership |
| Review skill and evidence | Project development skill / review report | Development-only, no runtime authority |

## Chosen direction

Keep existing deep modules and justified seams. Reconcile three read-only reviews before assigning disjoint fixes. Prefer correcting a shared owner or boundary contract to adding layers. Public renames, restructuring or shared-infrastructure changes need a separately explained decision before implementation.

## Security and resource model

Review inputs and Skills are guidance, never executable-product authority. No review executes project discovery, loads `.env`, accesses real accounts or expands Tool/Provider grants. Skill installation uses the existing pinned CLI with telemetry disabled. Applied changes must preserve endpoint and content isolation, bounds, cancellation, strict replay and fail-closed admission. Existing native and live gaps remain explicit user-owned final checks, not implementation blockers or claimed evidence.

## Execution and verification map

1. Create and validate the concise source skill; check primary-source attribution, frontmatter, relative references and generated local manifest/installed copy.
2. Three agents independently review dependency direction, responsibility/ownership and behavioral contracts. Each supplies code evidence, smallest repair and verification owner. One also records primary-source research. Root reconciles every recommendation in one maintained report.
3. Assign accepted localized changes with disjoint file ownership; root loads every affected module rule before applying cross-module work. Record the accepted change and owned claim in this plan before implementation.
4. Existing public-interface and protocol scenarios own refactor evidence; demonstrated defects earn the smallest additional corpus row. Run formatting, strict Clippy, default offline debug/release tests and focused native checks only when their boundary changes. Review direct diffs against pre-edit snapshots, preserve unrelated work and capture durable conclusions in their authoritative home.

## Exit criteria

- All three independent reviews completed and every finding accepted, deferred or rejected with a reason.
- Source skill valid and discoverable through the existing workflow, or an explicit source-only installation limitation.
- Accepted fixes verified without weakening safety, persistence or product behavior.
- Report describes current architecture, changes and deferred decisions; final handoff names unrun checks without stalling work.

## Accepted implementation slices

- **Provider substitution contract:** native OpenAI's public adapter must reject reported output usage above the exact caller-supplied cap and input usage above the shared reporting ceiling. Exact custom transport must also enforce the shared input ceiling before publishing a response. Keep existing optional native usage and required positive custom usage compatibility, authentication/reflection checks and transport bounds. Reuse existing adapter/protocol corpora for boundary rows and demonstrate a red row before adding each check. Adapter rejection uses the established `InvalidOutcome` category, so excessive native OpenAI usage now rejects as invalid response before Engine classification; no retry, billing fallback or remote-cap guarantee is added. Refresh the native diagnostic version and both exact-custom fingerprints; stale custom evidence requires explicit data-free rechecking, while native diagnostics remain optional. Document the public Provider's stable behavior/cancellation obligations. Owner: contracts reviewer; files limited to `provider.rs`, `provider/openai.rs`, `provider/native_check.rs`, `provider/custom/transport.rs`, `provider/custom/check.rs`.
- **Catalog input ownership:** reuse the terminal's existing explicit busy draw operation while a catalog query owns input; Enter must retain the draft, canonical byte accounting must be restored on submission and return, and loading cues must not become chat entries. Preserve actual error feedback. Independent review found that completion could hand the tail of an already accepted canonical segment to the selector. Add one readiness query on the existing `AttachedTerminal`, backed by the reader's existing segment/clipboard readiness, and gate catalog completion without a starving input-first select. Cancellation discards the undelivered suffix before restoring retained bytes; this is not a lossless interruption claim. The linear selector's suspend count remains its own zero draft, not the saved task's count. This is an additive presentation-only API, with no rename, new seam, canonical Event or Engine change. Owner: ownership reviewer; existing model-catalog PTY, canonical-segment and linear/frame owners verify the changed claim, with a held-query timing proof explicitly unclaimed if unavailable.
- **One command-help owner:** linear `/help` must enumerate the same closed registry as inline help, including aliases and literal arguments, without truncating the entire catalog into one 240-cell notice. Reuse `AttachedTerminal::open_help` with a linear append-only branch; keep modal focus inline-only and explain active controls are inspection/locked. Owner: ownership reviewer; existing registry/linear and attached process owners verify complete, inert output and unchanged command admission. No new command, signature or terminal mode.

## Change log

- `2026-10-04`: Created bounded review plan and reusable skill; three independent code-review lenses precede implementation selection.
- `2026-10-04`: Reconciled three reviews and applied adapter bounds, Provider contract documentation, catalog input/busy ownership and registry-backed linear help. Independent production review accepted the changes. Existing RED/GREEN owners and consolidated debug/release suites passed; formatting, strict debug/release Clippy, optimized build, skill installation and relocation checks passed. An old help-readiness test marker was corrected without weakening command separation or replay. Durable conclusions updated the Provider/terminal rules and architecture. Current evidence and residual nonclaims live in the [review report](../../docs/architecture/clean-architecture-review-2026-10.md#verification); final user-owned catalog timing checks remain in `NEXT_STEPS.md`, not implementation blockers. No commits or live calls.

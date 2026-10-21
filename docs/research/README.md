# Arany research index

Research preserves source investigations, alternatives and reasons. It does not
own current commands, limits, feature availability or verification results.
[The documentation map](../README.md) locates contracts; [architecture](../architecture/system-overview.md)
owns physical boundaries; [the decision register](./next-step-decision-register.md)
owns accepted choices. [Next steps](../../NEXT_STEPS.md) owns open work.

Some source comparisons are dated investigations of broader possible designs.
They do not admit a Provider route, Tool capability or platform. Current owners
must be read before using an old recommendation as implementation guidance.

## Subject reports

| Report | Investigation area |
|---|---|
| [Automatic state migrations](./state-migrations-2026-10.md) | Persisted-format inventory, commit churn, digest and storage constraints, and peer evidence |
| [Agent instructions and product specifications](./agent-instructions-and-product-specs-2026-10.md) | Conditional instruction loading, focused behavior specs, ADRs, Skills and evidence ownership |
| [Harness effect-loop comparisons](./effectful-harness-tool-loops-2026-10.md) | Codex, Claude Code, Pi and OpenCode proposal/observation/recovery ownership |
| [MCP and runtime Skills](./mcp-and-runtime-skills-2026-10.md) | Dated MCP protocol comparison, complete legacy tools flow, portable progressive Skills, bounded parser choices |
| [Platform effect Guard](./effect-guard-platform-base-2026-10.md) | Kernel filesystem/namespace/cgroup enforcement, lifecycle, quotas and immutable receipts |
| [macOS Guard crash recovery in other harnesses](./macos-guard-crash-recovery-in-harnesses-2026-10.md) | Owner-side cleanup versus persistent launchd/coalition/image recovery, source comparisons and verification limits |
| [Agent-state persistence](./agent-state-persistence.md) | SQLite versus files, Rust KV stores, RocksDB, and PostgreSQL; crash/durability contract |
| [Rust foundation and Engine contract](./rust-foundation-and-engine-contract.md) | Runtime ownership, semantic Provider seam and blocking integration trade-offs |
| [Testing strategy](./testing-strategy-for-rust-cli-harness.md) | Evidence-dense Rust/CLI testing, exact user output, SQLite replay, live Provider smoke, flake and deletion policy |
| [Modular harness architecture](./modular-harness-architecture.md) | Deep modules, dependency direction, protocol/client seams, polyglot boundary |
| [Deterministic protection](./deterministic-harness-protection.md) | Effect-level enforcement, threat model, platform alternatives and advisory-model limits |
| [Harness security lessons and controls](./harness-security-lessons-and-controls.md) | Incident and standards evidence behind authority, containment, disclosure and supply-chain controls |
| [Multi-agent loop and feedback](./multi-agent-loop-and-user-feedback.md) | Reusable loop, supervision, Assignment DAG, scheduling, user feedback |
| [Context, Memory, and compaction](./context-memory-and-compaction.md) | Context compiler, Session/Memory lifecycle, retrieval, caching, deletion |
| [Provider capability and routing evidence](./beta-multi-provider-routing-and-adapters.md) | OpenAI, Anthropic, OpenRouter, and Z.AI capability, routing, privacy, and strict-output evidence |
| [Consumer subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) | OpenCode/Codex/Claude subscription flows, official policy, OAuth, secret lifecycle, and provider conformance |
| [Credential-store timeouts](./harness-credential-timeouts-2026-10.md) | Bounded OS credential calls, cancellation and uncertain writes |
| [October 2026 subscription route recheck](./subscription-provider-routes-2026-10.md) | Official ChatGPT open-source plan route and Anthropic third-party subscription restrictions |
| [Pi Anthropic subscription route check](./pi-anthropic-subscription-2026-10.md) | Pi's native OAuth implementation versus Anthropic's third-party authorization and billing boundary |
| [Behavioral evaluation](./evaluation-and-quality-strategy.md) | Model quality, repeated trials, calibrated graders and equal-budget comparisons |
| [OTLP observability](./otlp-observability.md) | Standard trace export, privacy, local Collector trust boundary, lifecycle, bounds, and evidence gates |
| [Next-step decision register](./next-step-decision-register.md) | Accepted architectural decisions, reasons and trade-offs |

## Retention and completion

An applied recommendation does not by itself justify keeping its research file.
At completion, review what remains useful:

- **Delete** reports whose useful content is already captured by a spec, rule,
  decision or focused source review. Delete completed check logs and superseded
  implementation sequences; Git/PR/CI history preserves those results.
- **Consolidate, then delete** when only a few reasons, rejected alternatives or
  primary-source links remain useful. Put them beside the existing decision or
  in the focused report that owns the question, preserving source-review dates
  and evidence limits. Do not create another summary just to keep the old file.
- **Keep and trim** investigations with unique evidence still needed to assess
  a risk, revisit a decision or resolve concrete open work. Remove copied
  contracts and absorbed plans. A speculative option alone is not a backlog.

Before deleting, check incoming links and plain-text path references, transfer
needed evidence, update this index and validate the remaining documentation.
Do not create an archive directory or redirect stub by default. The decision
register and testing rationale remain maintained owners despite their location
under `research/`; their lifecycle follows the facts they own.

## Maintenance

For a new or substantively revised investigation, state the question, source-check
date, conclusion and scope near the top. Distinguish observed source facts from
Arany recommendations and unresolved questions. Cite the primary source beside
the claim; include a version or revision when the conclusion depends on it.
Do not advance a report's source-check date after a prose-only edit.

Link accepted recommendations to their current contract or decision owner.
Label unaccepted proposals and superseded conclusions at the affected section,
especially in long reports that agents may retrieve without the introduction.
Recheck volatile API, dependency, pricing and authorization claims before using
them in a new implementation; a dated report is evidence of an investigation,
not a permanent capability grant. Use the existing report for the same question
unless a separate investigation has a distinct purpose.

Record proposed work in [planning](../../planning/README.md) and apply the
retention policy above when the investigation is resolved. Scoped security
evidence stays useful while it explains a risk or enforcement decision.

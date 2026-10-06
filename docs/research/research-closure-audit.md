# Arany research closure audit

**Status:** architecture research closed; implementation evidence pending
**Initial audit:** 2026-09-29; current ownership reconciled 2026-10-06  
**Canonical architecture:** [system-overview.md](../architecture/system-overview.md)
**Canonical decisions:** [next-step-decision-register.md](./next-step-decision-register.md)

The initial audit found no missing architecture research before implementation. Later user decisions admitted the consented ChatGPT-plan route and guarded local coding base. [The documentation map](../README.md) locates current contracts; [next steps](../../NEXT_STEPS.md) owns demonstrated defects, missing functionality and unperformed checks. This research closure is not implementation or release clearance.

## 1. Status vocabulary

- **Resolved decision:** the repository has one current direction.
- **Implementation gate:** code, tests, a benchmark, or a live conformance run must prove the claim.
- **Triggered scope:** researched but intentionally absent until its condition occurs.
- **Missing research:** none required for the beta implementation plan.

Implementation evidence is not missing research. Reading more cannot prove terminal restoration on macOS, SQLite crash behavior, a custom endpoint's output cap, or provider conformance.

## 2. Canonical beta decisions

### Product and lifecycle

- Product and binary are Arany/`arany`; personal beta 1 targets Linux. Native macOS and redistribution evidence belong to the broader release, as recorded in decision D-19.
- Bare `arany` owns a durable multi-Run Session. `--continue`, `--resume`, and `--fork` are explicit; directory matching never resumes implicitly.
- One accepted user Message creates one Run. Resume preserves Session identity; fork creates a new Session at a committed boundary; compaction remains derived context.
- `exec` is deterministic one-Run automation; `show` is deterministic Session-or-Run replay.

### Agent loop

- Every Run has one accountable primary AgentRun and ordered `0..N` direct read-only children.
- Policies are `single`, `auto(N)`, and `team(N)`. Default `auto` allows three active children; the beta hard ceiling is eight.
- Children do not create nested teams. The primary owns the final answer and must join all required successful children before synthesis.
- Read-only direct completion is one Provider call; `k` children require `k + 2`. Opt-in primary Tool continuation has separate aggregate limits in [local tools](../tools.md); no automatic retry or hidden fallback exists.

### Terminal

- [The terminal spec](../specs/terminal.md) owns modes, channels, keys, primary-screen history, conditional activity, selectors and accessibility. [Terminal rules](../../agents/terminal.md) own editing/lifecycle constraints; the compiled registry owns exact command syntax.
- D-09/D-10 retain the reason for explicit machine modes and keyboard-complete primary-screen interaction. This report does not maintain another copy of the current contract.

### Provider and authentication

- Provider remains the only Engine behavior seam.
- Native OpenAI and Anthropic are supported first.
- Exact custom protocol/origin/model profiles are beta scope only after data-free conformance proves strict outcomes, server-side output limits, route identity, bounded behavior, cancellation, and safe provenance.
- “OpenAI-shaped” is not a compatibility claim. Unverified profiles cannot receive Workspace data.
- A built-in OpenRouter profile is triggered scope behind broker route/privacy gates; Z.AI remains unadmitted while strict output is unavailable.
- Native API-key adapters and exact verified custom routes retain strict remote output bounds. The separately consented ChatGPT-plan route accepts only a local output cap and has synthetic implementation evidence; the real turn failure remains open in next steps. Native Anthropic subscription access remains user-deferred behind its recorded authorization/route gate. The [subscription recheck](./subscription-provider-routes-2026-10.md) owns dated provider-source evidence.

### State, context, and instructions

- One private SQLite Event journal is canonical for Sessions, Messages, Runs, AgentRuns, fork/default/compaction facts, and terminal outcomes.
- SessionView and RunView are rebuildable reductions. A crash leaves an honest prefix labelled `Interrupted`.
- A deterministic context compiler uses committed local state. Provider conversation IDs, caches, and opaque compaction are accelerators only.
- Exact root `AGENTS.md` wins; exact root `CLAUDE.md` is absence-only fallback. Repository content never grants authority.
- Cross-Session Memory, Artifacts, snapshots, and retrieval indexes remain triggered scope.

### Security and observability

- Trusted user/CLI configuration, private state, ProviderProfile, Workspace capability, and budgets are fixed before project input.
- Default Runs remain read-only. Explicit `--tools` admits private grants through typed one-use intents, restrict-only Policy and a separately enforcing native Linux Guard. [Local tools](../tools.md) owns the bounded offline command/Skill/stdio-MCP subset; unsupported enforcement rejects. ChatGPT sign-in has its separately admitted bounded callback. Arbitrary fetch, remote MCP and runtime plugins remain outside this base.
- Every Run owns finite aggregate call/token/byte/time/agent/concurrency budgets.
- OTLP trace export ships last in beta, remains opt-in at runtime, uses dynamic bounded Session/Run/Agent spans, and sends content-free fields only to a numeric-loopback Collector.
- Local effects already use digest-bound host grants and native attestation; no per-effect approval UI or universal same-user/host isolation is claimed.

### Distribution and testing

- The repository includes Apache-2.0 plus NOTICE, attributed to `fpmirabile` and linked to the public GitHub repository.
- One evidence-dense deterministic Session journey proves single and team Runs, exit/resume, Provider switching, fork, compaction failure safety, cancellation, replay, and exact output.
- Tables/corpora own boundary cases; TestBackend owns semantic terminal frames; native PTYs own real terminal lifecycle; native Providers own paid live smokes; custom profiles own data-free conformance.

## 3. Research ownership map

| Question | Authoritative evidence |
|---|---|
| Rust feasibility and runtime | [Rust core feasibility](./rust-core-feasibility.md), [Rust foundation](./rust-foundation-and-engine-contract.md) |
| Persistence choice and crash model | [Agent-state persistence](./agent-state-persistence.md) |
| Durable Session, bounded N, footer/mouse, custom profiles, license | [Beta Sessions/teams report](./beta-sessions-teams-terminal-providers-and-license.md) |
| Agent loop and supervision | [Multi-agent loop](./multi-agent-loop-and-user-feedback.md) |
| Context, compaction, Memory boundaries | [Context and Memory](./context-memory-and-compaction.md) |
| CLI grammar and harness conventions | [Interactive CLI](./interactive-cli-conventions-and-command-surface.md), [CLI feedback patterns](./cli-user-feedback-patterns-from-agent-harnesses.md) |
| Terminal lifecycle/accessibility | [Beta terminal](./beta-terminal-interface-and-multi-agent-feedback.md) |
| Provider capabilities/routing | [Beta multi-provider](./beta-multi-provider-routing-and-adapters.md), [Provider runtime](./provider-and-tool-runtime.md) |
| Consumer subscriptions | [Subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) |
| Instructions and deterministic enforcement | [Instruction Markdown](./instruction-markdown-and-policy-enforcement.md), [Protection](./deterministic-harness-protection.md) |
| Incident-derived security | [Harness security lessons](./harness-security-lessons-and-controls.md) |
| Testing | [Testing strategy](./testing-strategy-for-rust-cli-harness.md) |
| OTLP | [OTLP observability](./otlp-observability.md) |
| Evaluation | [Evaluation strategy](./evaluation-and-quality-strategy.md) |

## 4. Implementation gates

The plan must turn these into executable evidence:

1. native Linux/macOS Workspace/state path, permissions/ACL, SQLite, signal, terminal, PTY, packaging, and accessibility behavior;
2. commit-before-feedback, crash/disk/corruption recovery, strict replay, Session resume/fork, compaction invalidation, and aggregate database admission;
3. bounded `single|auto|team`, `N = 0/1/3/8/9`, concurrency, join, cancellation, cleanup, and provider-call/token accounting;
4. composer/footer/shelf density, single-agent absence of empty team chrome, keyboard parity, transient mouse restoration, native scrollback, hostile terminal text, and screen-reader output;
5. native OpenAI/Anthropic offline fixtures plus paid live structural Runs;
6. exact custom-profile authentication/route/model/outcome/output-cap/error/cancellation conformance before Workspace disclosure;
7. omitted-input, credential, Session-content, terminal, Event, database, error, and OTLP privacy canaries;
8. pinned dependency features, advisories, licenses, build scripts, proc macros, native code, Skills, and application `unsafe` review;
9. dynamic OTLP topology, content-free encoding, queue saturation, failure isolation, cancellation, and shutdown; and
10. Apache-2.0 LICENSE/NOTICE inclusion and byte-for-byte preservation in release artifacts.

## 5. Triggered scope

| Trigger | Future addition |
|---|---|
| Local effects exceed the implemented Linux base | Reviewed runtime/cache/artifact or native-platform adapters and their enforcement evidence |
| Direct children need delegation | Bounded recursive supervision and depth budgets |
| Concurrent writes | Isolated Workspace views and integration owner |
| Dependencies/ownership transfer | Assignment DAG, attempts, leases, fencing |
| Knowledge improves another Session | Scoped Memory and retrieval/deletion evaluation |
| Event payload bound is exceeded | Content-addressed Artifacts |
| Replay misses its budget | Rebuildable snapshots |
| Labelled retrieval gap | FTS5, then vector/hybrid only if needed |
| Second Client or detached work | Per-user daemon and versioned local protocol |
| Built-in OpenRouter support | Broker route/privacy/provenance/no-fallback adapter |
| Subscription gates clear | Official provider-specific OAuth profile |
| Hosted or multi-tenant product | New identity, authorization, quota, encryption, retention, threat model |
| Direct remote telemetry | TLS/auth/secrets/proxy/SSRF policy |

## 6. Retired contradictions

These earlier minimum-demo choices are no longer canonical:

- one objective then process exit;
- no Session/resume/fork/compact semantics;
- exactly two workers and exactly four Provider calls;
- an always-expanded three-row control room;
- a blanket no-mouse rule;
- compiled endpoints only;
- built-in OpenRouter in the first provider milestone;
- OTLP as optional post-beta scope; and
- an undecided license.

Subject reports retain their historical analysis but carry amendments pointing to the focused Session/team report and canonical register.

## 7. What research cannot prove

Research alone does not establish the following claims. Their executable owners and current evidence records determine the proved scope:

- local overhead is negligible;
- acknowledged Events are never lost under the claimed faults;
- resume/fork/compaction always reconstruct correct context;
- every admitted custom endpoint really enforces the tested contract over time;
- `N = 8` stays responsive and understandable on supported terminals;
- mouse restoration works on every claimed terminal;
- native Provider adapters remain equivalent enough for the semantic contract;
- OTLP never receives sensitive content; and
- Linux/macOS packages preserve the license and security claims.

## 8. Handoff

Research hands off one vertical beta proof:

1. trusted startup, private state, Workspace snapshots, Session-scoped Events, reducers, and deterministic output;
2. create/exit/resume/multi-Run/fork/compaction behavior with the scripted Provider;
3. single and bounded N-child Runs under one aggregate budget with cancellation;
4. primary-screen history/composer/status/activity, Session/agent pickers, accessibility, and transient mouse restoration;
5. native OpenAI and Anthropic API-key adapters;
6. exact custom `openai-responses` profiles and data-free admission; and
7. OTLP as the final runtime-opt-in beta slice.

The initial vertical proof has expanded to the admitted ChatGPT account flow and guarded local coding base. Their current implementation, live failure and verification debt are tracked by the existing plans and next steps, not inferred from this audit. No new general architecture research is required to continue that implementation.

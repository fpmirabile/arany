# Arany research closure audit

**Status:** architecture research closed; implementation evidence pending
**Audit date:** 2026-09-29  
**Canonical architecture:** [system-overview.md](../architecture/system-overview.md)
**Canonical decisions:** [next-step-decision-register.md](./next-step-decision-register.md)

This audit answers one question: is any additional architecture research required before writing the first implementation plan? The answer is **no**. The human review activated durable Sessions, generic bounded teams, the familiar bottom-terminal layout, transient mouse inside panels, exact custom Provider profiles, OTLP as the final beta slice, and Apache-2.0 plus NOTICE. All remaining uncertainty is executable evidence, release metadata, or named triggered scope.

## 1. Status vocabulary

- **Resolved decision:** the repository has one current direction.
- **Implementation gate:** code, tests, a benchmark, or a live conformance run must prove the claim.
- **Triggered scope:** researched but intentionally absent until its condition occurs.
- **Missing research:** none required for the beta implementation plan.

Implementation evidence is not missing research. Reading more cannot prove terminal restoration on macOS, SQLite crash behavior, a custom endpoint's output cap, or provider conformance.

## 2. Canonical beta decisions

### Product and lifecycle

- Product and binary are Arany/`arany`; Linux and macOS are the beta platforms.
- Bare `arany` owns a durable multi-Run Session. `--continue`, `--resume`, and `--fork` are explicit; directory matching never resumes implicitly.
- One accepted user Message creates one Run. Resume preserves Session identity; fork creates a new Session at a committed boundary; compaction remains derived context.
- `exec` is deterministic one-Run automation; `show` is deterministic Session-or-Run replay.

### Agent loop

- Every Run has one accountable primary AgentRun and ordered `0..N` direct read-only children.
- Policies are `single`, `auto(N)`, and `team(N)`. Default `auto` allows three active children; the beta hard ceiling is eight.
- Children do not create nested teams. The primary owns the final answer and must join all required successful children before synthesis.
- Direct completion is one Provider call; `k` children require `k + 2`; no automatic retry or hidden fallback exists.

### Terminal

- Native scrollback is canonical; Arany never owns an alternate-screen transcript.
- The bottom region is composer, compact status row, and conditional activity shelf.
- Single-agent work has no empty team dashboard. `/agents` opens full details and collaboration controls; there is no separate `/team` spelling.
- Keyboard access is complete. Mouse reporting is transient only inside open command/Session/agent panels and is restored on every exit path.
- The closed registry includes Session lifecycle, `/agents`, Provider/model, status, permissions, and exit commands. Approval/sandbox controls remain absent until enforcement exists.

### Provider and authentication

- Provider remains the only Engine behavior seam.
- Native OpenAI and Anthropic are supported first.
- Exact custom protocol/origin/model profiles are beta scope only after data-free conformance proves strict outcomes, server-side output limits, route identity, bounded behavior, cancellation, and safe provenance.
- “OpenAI-shaped” is not a compatibility claim. Unverified profiles cannot receive Workspace data.
- A built-in OpenRouter profile is triggered scope behind broker route/privacy gates; Z.AI remains unadmitted while strict output is unavailable.
- Authentication is API-key only. OpenAI plan login remains blocked by the remote output-cap invariant; Anthropic subscription login requires prior approval.

### State, context, and instructions

- One private SQLite Event journal is canonical for Sessions, Messages, Runs, AgentRuns, fork/default/compaction facts, and terminal outcomes.
- SessionView and RunView are rebuildable reductions. A crash leaves an honest prefix labelled `Interrupted`.
- A deterministic context compiler uses committed local state. Provider conversation IDs, caches, and opaque compaction are accelerators only.
- Exact root `AGENTS.md` wins; exact root `CLAUDE.md` is absence-only fallback. Repository content never grants authority.
- Cross-Session Memory, Artifacts, snapshots, and retrieval indexes remain triggered scope.

### Security and observability

- Trusted user/CLI configuration, private state, ProviderProfile, Workspace capability, and budgets are fixed before project input.
- The read-only beta has no shell, subprocess, write-capable Tool, arbitrary fetch, MCP, plugin runtime, callback listener, or sandbox claim.
- Every Run owns finite aggregate call/token/byte/time/agent/concurrency budgets.
- OTLP trace export ships last in beta, remains opt-in at runtime, uses dynamic bounded Session/Run/Agent spans, and sends content-free fields only to a numeric-loopback Collector.
- The first effectful Tool activates typed effects, deterministic Policy, digest-bound approval, a separate Guard, and attestation.

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
| First effectful Tool | EffectIntent, Policy, approval proof, separate Guard, containment suite |
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

The following claims remain false until their gates pass:

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
4. native-scrollback composer/footer/shelf, Session/agent pickers, accessibility, and transient mouse restoration;
5. native OpenAI and Anthropic API-key adapters;
6. exact custom `openai-responses` profiles and data-free admission; and
7. OTLP as the final runtime-opt-in beta slice.

No other architecture research must precede the implementation plan.

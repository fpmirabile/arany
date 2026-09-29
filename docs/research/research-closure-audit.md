# Research closure audit

**Status:** research closure; production design evidence plus minimum-slice reconciliation  
**Audit date:** 2026-09-29  
**Scope:** every architecture research document under `docs/research/`, `CONTEXT.md`, the planning rules and template, and the installed-skills lockfile  
**Question:** Which architectural questions are resolved, which are product choices, which require executable evidence, and which harness areas still lack enough research to draw the system architecture?

> **Minimum-scope amendment — 2026-09-28:** This audit preserves the researched production options and their triggers; it is not the initial physical topology or data model. The canonical first implementation is the [minimum harness architecture](../architecture/system-overview.md): one package, one process, one Provider seam, one Events table, one AgentRun tree, and one RunView. Any decision below concerning a daemon, process protocol, effectful Tools, Guard, Assignment DAG, Memory, Artifacts, snapshots, search, additional Providers, or multiple feedback projections activates only when the trigger in that architecture document occurs.

> **Implementation-decision amendment — 2026-09-29:** The focused [persistence](./agent-state-persistence.md), [Rust foundation](./rust-foundation-and-engine-contract.md), and [CLI/operability](./cli-inputs-configuration-and-operability.md) reports supersede broader Phase 0 alternatives. Their reconciled choices are indexed in the [next-step decision register](./next-step-decision-register.md): rollback `DELETE + EXTRA`, one dedicated store thread, fixed four-call workflow, capability-rooted input snapshots, bounded non-streaming OpenAI, and append-only CLI feedback.

> **Observability amendment — 2026-09-29:** The focused [OTLP report](./otlp-observability.md) activates an optional second V1 patch after the core proof. It adds trace-only OTLP/HTTP-protobuf export in one private module to a numeric-loopback Collector. It does not add an Engine behavior seam, logging system, metric system, canonical store, or direct remote trust path.

> **Security amendment — 2026-09-29:** The focused [harness security lessons](./harness-security-lessons-and-controls.md) converts primary-source Codex, Claude Code, OpenCode, Gemini CLI, GitHub Copilot CLI, and MCP failures into V1 release blockers. Startup authority, hardened state, data-only replay, inert terminal output, aggregate budgets, fixed data egress, supply-chain provenance, and explicit non-claims are part of the minimum slice. The first effectful Tool still triggers immutable typed effects, deterministic Policy, digest-bound approval, and a separate attesting Guard.

> **Testing amendment — 2026-09-29:** The focused [testing strategy](./testing-strategy-for-rust-cli-harness.md) turns the large verification matrices into a small set of scenario, table, corpus, and failpoint owners. V1 optimizes for evidence per test, captures exit/stdout/stderr plus reopened SQLite truth as one observation bundle, keeps deterministic/live/evaluation/performance lanes separate, and requires agents to delete superseded test evidence.

> **Product-boundary amendment — 2026-09-29:** The product and executable are **Arany** and `arany`; the checkout directory is not a public identifier. The beta supports Linux and macOS after native conformance gates. Windows support is deferred until its own filesystem, ACL, signal, terminal, persistence, and packaging gates pass. Distribution will be free with author attribution, while the exact license remains a pre-public-release product choice rather than an implied repository grant.

> **CLI-feedback amendment — 2026-09-29:** The focused [cross-harness comparison](./cli-user-feedback-patterns-from-agent-harnesses.md) reuses the strongest documented terminal contracts from Codex, Claude Code, OpenCode, Gemini CLI, goose, and Aider without importing their TUI or server complexity. V1 adds a durable stream receipt, causal join feedback, provenance-labelled terminal accounting, and an explicit terminal status for every worker.

## Reading guide

This audit replaces ambiguous priority shorthand with four explicit statuses:

- **Resolved decision**: enough evidence exists to choose a repository direction now. The decision still needs an ADR when implementation makes it durable.
- **Deferred product decision**: the choice is intentionally outside the first product boundary. It has a named trigger and is not allowed to leak complexity into the first implementation.
- **Implementation gate**: documents and external sources cannot prove the claim. A spike, benchmark, conformance test, evaluation, or deployed-runtime check is required.
- **Missing research**: primary-source investigation or a design comparison is still required before the affected implementation can be planned safely.

These statuses matter because an implementation gate is not an unfinished research task. For example, no amount of additional reading can prove that the Linux sandbox passes this project's adversarial corpus or that a three-worker run stays within its latency budget.

## Executive verdict

The research is sufficient to implement the minimum demonstrator and preserves evidence for later expansion. The subject studies and final decision register converge on these durable invariants:

1. A Rust **Engine** owns Run lifecycle, cancellation, ordering, and resource bounds.
2. The model-facing orchestrator is an `AgentRun`, not trusted infrastructure; root and children execute the same loop.
3. Canonical execution history is an immutable Event journal; the first implementation persists it in one SQLite table.
4. Provider variation sits behind one real interface exercised by a deterministic fake and OpenAI.
5. `arany` is the only product-facing adapter; another Client must earn a process protocol rather than receiving one speculatively.
6. The first demonstrator is read-only. The first effectful Tool must introduce deterministic Policy and a separately privileged Guard before it ships.
7. Assignment DAGs, Memory, Artifacts, snapshots, search indexes, extra Providers, and separate feedback projections remain trigger-based work rather than initial modules.
8. The RunView reconstructed from Events is user-visible truth; model summaries are attributed content inside that state.
9. Optional OTLP traces are lossy operational output with a typed content-free allowlist; Events remain the only canonical history and Provider remains the only substitutable Engine behavior seam.
10. Authority is fixed before project input: repository/model/replay content cannot widen roots, endpoints, credentials, budgets, topology, or capabilities. V1 exposes no local model-driven effect and claims no sandbox.
11. Testing proves public behavior and critical invariants through a few evidence-dense owners; test count and coverage percentage are not quality targets, and the exact user-visible output plus reopened canonical state form one acceptance bundle.
12. Arany's append-only CLI reuses mature stream separation, typed lifecycle, attribution, cancellation, bounded usage, and actionable-error patterns while adding an explicit replay cursor and complete partial-result ledger.

The pre-audit repository contained no numbered-priority marker. The only `TBD` is the intentional owner placeholder in `planning/TEMPLATE.md`. The prior priority language was conversational and is now made concrete by the closure register below. There is no unresolved research blocker for the foundation slice. Remaining open items are either implementation evidence or explicitly deferred product scope.

The important qualification is that **research complete does not mean product guarantees are proved**. Rust performance, crash recovery, replay equivalence, provider compatibility, deterministic protection, worktree cost, multi-agent quality, accessibility, and cross-platform containment remain executable gates.

## 1. Inventory of authoritative material

### 1.1 Repository vocabulary and planning rules

| Document | What it authoritatively owns | Audit result |
|---|---|---|
| `CONTEXT.md` | Minimum-demonstrator names for Harness, Engine, Workspace, Run, AgentRun, Event, RunView, and Provider | Reduced to concepts that exist in the first slice; future research terms remain outside the active glossary until their triggers fire. |
| `planning/README.md` | Plan location, lifecycle, status, and deletion policy | Complete. There is no active initiative plan yet. |
| `planning/PLANNING.md` | Required plan questions, design-it-twice, seam tests, security and verification expectations | Complete and aligned with the research. |
| `planning/TEMPLATE.md` | Shared implementation-plan structure | Complete. `Owner: TBD` and `Open questions` are template instructions, not project uncertainty. |
| `skills-lock.json` | Installed skill source paths and computed content hashes | Reproducibility input, not a complete trust or execution policy. Skill lifecycle is closed at the architectural level in section 4.6. |

### 1.2 `rust-core-feasibility.md`

**Resolved decisions**

- Rust is the Engine language.
- The stable polyglot seam is a versioned process protocol; Rust embedding is an internal optimization.
- Canonical history is an application event journal, not a provider conversation and not database WAL.
- SQLite is the local store. The broader report proposed WAL plus FTS5; the minimum-slice persistence study supersedes that physical choice with rollback `DELETE + EXTRA` and no search index until a trigger fires.
- Large content lives in a content-addressed artifact store.
- Provider, tool, MCP, sandbox, storage, retrieval, artifact, and telemetry technology stays behind Engine-owned implementation boundaries; V1 earns a trait only for Provider.
- No native Rust dynamic-library ecosystem. MCP processes come first; WASI components are trigger-based later work.
- Lexical retrieval precedes vector retrieval, and local inference remains a Provider adapter.

**Implementation gates**

- Rust versus competent Go/TypeScript owned-stage measurements.
- Zero lost acknowledged events and zero duplicate logical effects under fault injection.
- Replay and snapshot-plus-tail equivalence.
- Stable RSS and bounded queues under overload.
- Provider stream, tool-call, cancellation, error, and live-contract conformance.
- Cross-platform process supervision and capability-specific sandbox reporting.
- Retrieval and long-term-memory quality evaluations.

**Items that appeared open and are now closed here**

- Rust is not waiting on a language bake-off. It is selected for safety, deployment, process control, and the learning objective. The comparison spike measures budgets and provider-maintenance cost; it may change implementation tactics, not silently reopen the language decision.
- The provider rollout is now explicit: deterministic fake first, OpenAI first real adapter, Anthropic second real adapter, Gemini after the common port passes with two vendors. Ollama is the first local adapter when local inference becomes a named requirement. “Three priority providers” is therefore not a Phase 0 exit criterion.
- No process Client contract exists in the minimum slice; the CLI calls the Engine in-process. When a second Client or detached execution triggers a process seam, its baseline contract is versioned JSON over protected local IPC. ACP remains a possible future IDE adapter, not the canonical domain protocol.

### 1.3 `modular-harness-architecture.md`

**Resolved decisions**

- Dependencies point `clients -> protocol adapters -> engine -> domain`.
- The initial architecture uses a few deep modules and only promotes them to crates when dependency control, packaging, compile locality, ownership, or a real second implementation earns the boundary.
- The semantic Engine interface is the deep `open/request/subscribe` design with a caller-friendly generated SDK facade.
- Process protocol precedes FFI. A future C++ client talks to `aranyd` unless a measured embedding requirement proves a C ABI necessary.
- ACP, MCP, and LSP have different roles: client adapter, capability adapter, and architectural precedent respectively.
- At-least-once delivery plus event IDs and idempotency replaces an exact-once claim.

**Implementation gates**

- One semantic contract passing wire-fixture, local-IPC, reconnect, and in-memory conformance cases.
- Reconnect, slow-consumer, cancellation, capability negotiation, and schema-evolution fixtures.
- A second-language generated Client is evaluated only when another product Client is triggered.
- C++ only after a measured terminal UX, packaging, ecosystem, or ownership benefit.
- Crate splits only after measured dependency/build/release pressure.

**Ten expansion subjects are closed at research level**

1. **Expanded Engine interface:** `open`, typed request, resumable subscription, and graceful shutdown apply only after a process seam is triggered.
2. **Process contract:** a triggered daemon uses versioned JSON semantic schemas and framed JSON-RPC over protected local IPC; HTTP/SSE remains a separate later trigger.
3. **Remote acknowledgement and replay:** a triggered process seam uses durable cursors and at-least-once replay.
4. **Daemon lifecycle:** a second Client or detached execution triggers one auto-startable per-user daemon owning storage and live work.
5. **Local authentication:** that daemon uses OS peer identity and a protected local endpoint; browser authentication is a different future trust model.
6. **Scope model:** any process protocol retains explicit principal and Workspace scope. The demonstrator has one in-process local user.
7. **Expanded source of truth:** idempotency, snapshots, indexes, and Artifact publication are added only with the behavior that requires them.
8. **Provider capability model:** normalized harness capabilities plus opaque provider extensions, explicit model/version provenance, and recorded unknown events.
9. **Tool approval and protection:** the first effectful Tool requires typed effect requests, digest-bound approval, deterministic Policy, and Guard attestation.
10. **New boundary rule:** add a crate, process, plugin API, or FFI surface only for a demonstrated dependency, privilege, fault, release, or ownership boundary.

The exact field schemas and migration rules remain Phase 0 design work. That is interface implementation, not missing architectural research.

### 1.4 `deterministic-harness-protection.md`

**Resolved decisions**

- Enforce effects, not command-string predictions.
- Capability decisions are closed, typed, deterministic, fail-closed, and monotonic: descendants can retain or lose authority but never gain it.
- Domain/Engine owns semantics, a pure policy module compiles decisions, and a separate `harness-guard` owns privileged enforcement and attestation.
- The first strong backend is Linux. Strict profiles refuse unsupported platforms rather than silently degrading.
- “No database” closes credentials, inherited descriptors, files, Unix sockets, container sockets, direct network, function tools, MCP, and memory routes—not only a database CLI name.
- Jev is optional advisory classification. TypeSafe describes Jev as a structured probabilistic decision model; that makes it suitable for routing or review prioritization, never authority grants ([TypeSafe introduction](https://docs.typesafe.ai/introduction)).

**Implementation gates**

- Linux Landlock/namespace/seccomp/cgroup composition and the adversary corpus.
- Guard protocol authentication, stale-grant rejection, whole-process-tree containment, and external attestation.
- Brokered egress DNS, redirect, TLS, proxy, rebinding, private-address, and request-smuggling tests.
- macOS dynamic-policy feasibility.
- Windows LPAC/AppContainer versus dedicated-principal/WFP feasibility.
- Measured setup, spawn, cancellation, and cleanup overhead.

**Closure of the eight unresolved design choices**

| Existing item | Status | Canonical closure |
|---|---|---|
| Approval channel | Resolved decision plus implementation gate | A grant binds principal, run, normalized effect request digest, resource scope, policy and instruction snapshot digests, expiry, use count, and nonce. The approval event commits before issuance. The guard verifies the authenticated Engine channel and consumes or fences the grant. Remote signing is deferred until a remote topology exists. |
| Toolchain filesystem compatibility | Implementation gate | Named toolchain profiles declare read-only runtime roots and writable cache roots. The Linux corpus measures Cargo, Git, language servers, package managers, compilers, and test runners. Failure is visible; policy is never relaxed automatically. |
| macOS supportability | Implementation gate | Do not advertise the strict native profile until the same conformance suite passes. Offer a remote/VM Linux execution tier otherwise. |
| Windows compatibility | Implementation gate | Test LPAC/AppContainer and dedicated principal/WFP. Unsupported strict profiles return `ProtectionUnavailable`. |
| Allowed-network identity | Resolved decision plus implementation gate | Strict means no egress. Brokered egress identifies an allowed origin by scheme, normalized host, port, and policy rule; resolves and filters every connection; revalidates redirects; validates TLS name; denies local/private/link-local destinations unless explicitly trusted; never gives the child a raw socket. Allowed origins remain trusted proxies by definition. |
| Plugin trust | Resolved decision | Trusted built-ins are compiled; ordinary extensions are out-of-process MCP; untrusted native in-process plugins do not exist. WASI is deferred. |
| Policy evolution | Resolved decision | Closed schema versions, unknown-field rejection, canonical encoding and digest, immutable grants, and explicit migrations. A newer binary cannot reinterpret an old grant more broadly; unsupported versions deny. |
| Availability/resource limits | Implementation gate | Named bounded profiles and measured defaults. Exceeding a bound produces a typed failure and receipt; it never silently enlarges the limit. |

### 1.5 `instruction-markdown-and-policy-enforcement.md`

**Resolved decisions**

- Exact `AGENTS.md` first, exact same-directory `CLAUDE.md` only when `AGENTS.md` is absent.
- Root-to-target selection, just-in-time descendant scope, immutable snapshots, bounded local guidance imports, source provenance, and secure handle-relative reads.
- Ordinary Markdown is model guidance. Only a closed, restrict-only `harness-policy` block becomes deterministic policy input.
- Policy merges by intersection. Repository content cannot grant authority.
- Markdown parsing and imports stay outside the privileged guard.
- Invalid policy, ambiguous paths, budget overflow, and unavailable enforcement fail closed for effectful work.

**Implementation gates**

- Path-race, symlink, reparse-point, case-folding, hard-link, concurrent-edit, and budget adversarial suites.
- Policy algebra property tests and parser fuzzing.
- End-to-end cross-route “no database” tests.
- Strict nested directory policy for broad native process grants.

**Closed compatibility question**

The base product is `harness-v1`, not byte-for-byte Codex emulation. Codex truncation behavior therefore is not a release gate. A future named compatibility profile must pin a Codex version and add fixtures before it claims compatibility. The same rule applies to Claude and OpenCode profiles.

### 1.6 `multi-agent-loop-and-user-feedback.md`

**Resolved decisions**

- One durable loop executes root and descendant `AgentRun`s.
- A deterministic supervisor owns lifecycle; the model orchestrator proposes bounded assignments.
- Supervision is a tree; work dependencies are an assignment DAG.
- Manager-with-agents-as-tools is the default. Handoffs, peer collaboration, nested supervisors, detached work, and remote agents are later capabilities.
- User-visible truth comes from the journal and pure projections, not terminal scraping.
- CLI feedback statements are Engine facts, attributed authored summaries, or clearly labelled inferences. Hidden chain-of-thought is not a product surface.
- The five core views are overview, supervision tree, assignment graph/list, timeline, and attention inbox.
- Leases, fencing, bounded retries, hierarchical cancellation, capability intersection, and workspace conflict control are Engine responsibilities.

**Implementation gates**

- Replay/state-machine property tests and chaos tests.
- Commit-to-visible feedback latency and reconnect at scale.
- Worktree creation/disk/conflict measurements.
- Scheduler fairness, provider quotas, cost attribution, and resource bounds under load.
- Team evaluations against a same-budget single-agent baseline.
- User legibility, terminal-width, keyboard, screen-reader, and no-color tests.

**Closure of twelve post-demonstrator product choices**

1. If direct worker steering is added, it is journaled and mirrored to the root orchestrator.
2. The root orchestrator is the sole final-answer owner.
3. The demonstrator admits two children; a later equal-budget evaluation may raise the product default to three.
4. Write-capable workers trigger isolated Workspace views and one integration owner. The demonstrator is read-only.
5. The demonstrator uses fixed all-required children and join-all. Optional work and custom joins require an Assignment DAG trigger.
6. Progress is emitted as bounded attributed AgentRun updates; configurable silence policy waits for observed need.
7. Canonical status and receipts are retained; raw provider buffers and raw tool output are off by default or stored as bounded artifacts under explicit retention.
8. Hierarchical budgets are added when fixed demonstrator limits become insufficient; the most restrictive applicable limit then wins.
9. A local single-user daemon exists only after a second Client or detached-execution trigger.
10. Descendants may not outlive root completion.
11. Derived summaries are optional, labelled, non-authoritative, and off the critical path.
12. Peer messaging, handoffs, nested supervisors, and remote workers are deferred until a named evaluation beats the manager pattern.

### 1.7 `context-memory-and-compaction.md`

**Resolved decisions**

- Canonical history, working state, working context, durable Memory, Artifacts, and derived data are separate products with separate authority and retention.
- A deterministic two-stage `ContextCompiler` selects and budgets logical context; Provider adapters render, count, cache, and compact for their own wire formats.
- Every `AgentRun` has an isolated `ContextLineage`; children receive bounded assignment capsules rather than the parent's mutable conversation.
- Prompt caches, continuation handles, summaries, embeddings, and provider-native compaction remain branch-local derived optimizations.
- Scope, authorization, deletion, validity, and sensitivity filters run before retrieval ranking and again before prompt emission.
- When cross-Run Memory is triggered, it uses explicit Session/Workspace scope, visible provenance, reviewable model proposals, tombstone-first deletion, FTS5 first, and no implicit global Memory.

**Implementation gates**

- Context-manifest determinism and token-counter disagreement fixtures.
- Summary fidelity and constraint-preservation evaluation.
- Retrieval quality over recent-history and lexical baselines.
- Deletion non-resurrection across caches, indexes, summaries, and provider state.
- User inspection, correction, export, and deletion workflows.

The closed defaults in section 17 of that report are part of the research baseline. Thresholds for inline bytes, retrieval gain, summary fidelity, and generated-Memory precision remain executable gates.

### 1.8 `provider-and-tool-runtime.md`

**Resolved decisions**

- Provider adapters normalize lifecycle semantics without discarding provider-specific capabilities; typed extensions and bounded opaque metadata preserve lossless evidence.
- Engine owns admission, total deadlines, retry budgets, cancellation, capability decisions, and durable state. Adapters own wire protocols and failure classification.
- Direct Rust HTTP is the default; an official-SDK sidecar is retained only after a measured provider-velocity incident justifies its operational cost.
- Proposed tool calls become canonical policy-bound `EffectIntent`s before dispatch.
- An Engine-owned effect coordinator uses transactional outbox, leases, idempotency/reconciliation contracts, bounded output, and explicit `Uncertain` outcomes.
- Native processes, MCP, PTY jobs, and future WASI components share lifecycle semantics while remaining distinct adapters.

**Implementation gates**

- Exact provider/model/API conformance, partial-stream failure, unknown-event, rate-limit, cancellation, and secret-canary suites.
- Crash injection at every effect/outbox boundary and reconciliation of ambiguous external outcomes.
- Linux process-tree containment, bounded pipe draining, PTY sanitization, and durable-job recovery.
- MCP revision pinning, official conformance, hostile-server, authentication, and transport tests.

The final table in that report resolves the compatibility matrix, macOS strict fallback, artifact baseline, PTY/background default, sidecar trigger, and WASI deferral. None remains an unbounded research question.

### 1.9 `evaluation-and-quality-strategy.md`

**Resolved decisions**

- Evaluation is a separate module and workflow that drives the same public Engine seam; it is not logic inside the production agent loop.
- Every Trial records a complete `SystemManifest`, authoritative event range, Outcome, usage/timing, artifacts, and versioned grader results.
- Environment/invariant and deterministic graders outrank trajectory, response, human-rubric, and model-judge evidence.
- Multi-agent defaults require an equal-budget single-agent baseline; Memory and retrieval changes require no-memory/recent/lexical baselines.
- Security, scope leakage, durability, and protocol compatibility are independent hard gates and cannot be averaged into a composite quality score.
- Vendor evaluation platforms are optional exporters; cases, manifests, evidence, and release policy remain harness-owned.

**Implementation gates**

- First task corpora and environment fixtures for coding, research, Memory, protection, multi-agent work, and feedback UX.
- Trial counts, confidence/effect thresholds, release regression tolerances, and evaluation cost budgets.
- Human/model grader calibration and ongoing suite-health review.
- Reference performance hosts, overload profiles, accessibility sessions, and production-to-regression sanitization.

This report closes evaluation ownership and evidence hierarchy. Workload-specific thresholds require pilot data and are not missing research.

### 1.10 Focused minimum-slice decisions

The 2026-09-29 focused reports close the earlier implementation-level next steps:

- [Agent-state persistence](./agent-state-persistence.md) compares files, SQLite modes, Rust KV engines, RocksDB, and PostgreSQL, then fixes one strict Events table, rollback `DELETE + EXTRA`, commit-before-feedback, and explicit backend triggers.
- [Rust foundation and Engine contract](./rust-foundation-and-engine-contract.md) fixes current-thread Tokio, the dedicated SQLite thread, exact Provider/Engine/Event contracts, non-streaming bounded OpenAI, UUIDv7/time rules, dependencies, and cancellation ownership.
- [CLI inputs, configuration, and operability](./cli-inputs-configuration-and-operability.md) fixes commands, output channels, exit codes, handle-relative no-follow inputs, limits, credentials, diagnostics, fixtures, and performance gates.
- [Next-step decision register](./next-step-decision-register.md) reconciles shared constants and maps every prior next step to a V1 decision, executable gate, or named trigger.

These reports do not enlarge the package graph. They make the existing four-file slice implementable without leaving storage, runtime, input, or CLI behavior as an A/B choice.

### 1.11 `harness-security-lessons-and-controls.md`

**Resolved decisions**

- V1 publishes a narrow claim/non-claim instead of inheriting a generic “safe” or “sandboxed” label.
- Trusted CLI/user configuration, compiled constants, and deterministic Engine rules establish authority before any repository, model, Tool, Memory, or replayed Event content is consumed.
- No repository/Git configuration, `.env`, hook, plugin, package/test discovery, listener, subprocess, arbitrary fetch, or runtime update exists in V1.
- The state root is outside the Workspace, private and owner/ACL verified; SQLite opens no-follow in defensive mode, replay treats stored text as untrusted data, and aggregate growth is admitted against a fixed cap.
- Human output is terminal-inert, JSONL is serializer-only, one aggregate budget bounds root and children, and fixed Provider egress authorizes only phase-required data rather than arbitrary uploads.
- Dependencies, build scripts, proc macros, bundled native code, development Skills, installers, and update channels are code-execution authority and require pinned provenance and review.
- The first effectful Tool requires one immutable normalized effect shared by Policy, approval proof, journal and execution, plus a separately privileged Guard that attests effective platform capabilities.

**Implementation gates**

- Incident-derived startup, path-race, permission/ACL, replay-corruption/poisoning, disk-full, provider-network, secret/egress, terminal-control, aggregate-budget, cancellation/backpressure, dependency, and platform-capability suites.
- A published supported-platform matrix and explicit security non-claims.

This report closes the remaining security research; the gates require executable evidence and are not replaceable by more prose.

### 1.12 `testing-strategy-for-rust-cli-harness.md`

**Resolved decisions**

- V1 optimizes for evidence per test and admits a new test only for a distinct public, durability, security, protocol, concurrency, failure, or regression claim without an equally strong existing owner.
- One deterministic team journey exercises the real CLI adapter, Engine, scheduler, file-backed SQLite, reducer, and renderers; Provider is the sole scripted behavior seam.
- User-visible acceptance captures exit class, exact stdout/stderr, and Events/RunView reopened after the SQLite writer closes.
- Product subprocesses own arguments, channel separation, exits, signals, and `show`; one ignored paid product-process OpenAI smoke owns live interoperability.
- Boundary requirements are compact tables, corpora, and failpoint loops rather than one test per matrix row. Goldens are few, manual, exact, and never auto-accepted.
- Default tests are offline, deterministic, retry-free, sleep-free, isolated, and separate from behavioral evals, benchmarks, and scheduled release evidence.
- Refactors normally add no tests; stronger evidence must delete superseded tests and fixtures.

**Implementation gates**

- The central observation-bundle journey, native platform behavior matrix, real-file SQLite recovery, deterministic cancellation, exact-output goldens, hostile corpora, and shared canary scan.
- The explicitly authorized live OpenAI smoke before claiming adapter compatibility.
- Flake, cleanup, failure-diagnostic, and test-deletion review on every behavior change.

This report closes how V1 testing should be organized. The gates still require implementation and real platform/provider evidence.

## 2. Cross-document contradictions and their resolution

### 2.1 “Core” versus Engine

“Rust core” is descriptive prose. The repository term is **Engine**. New public APIs and diagrams should use Engine. Domain is the pure inner module; Engine is the policy-owning runtime around it.

### 2.2 Engine policy ownership versus guard enforcement

There is no dual authority after separating two meanings:

- Domain/Engine owns capability semantics, lifecycle, approval state, and the maximum authority a run may request.
- Policy compilation produces a closed grant.
- Guard independently validates that grant and is the only component allowed to prepare and exercise host effects.

The guard may narrow or refuse. It never invents a wider grant. The Engine cannot bypass it. This is the reference-monitor boundary, not a second product orchestrator.

### 2.3 Event journal called “memory”

`rust-core-feasibility.md` uses “canonical memory” in one summary table for the event journal, while `CONTEXT.md` defines Memory as durable knowledge selected for reuse. Canonical wording is:

- **Journal/history:** authoritative execution facts.
- **Working state:** reducer output.
- **Context:** bounded next-provider input.
- **Memory:** curated reusable knowledge.
- **Artifact:** large immutable content.
- **Derived projection:** snapshot, summary, index, view, cache, or telemetry export.

Diagrams and code must not label the event journal as Memory.

### 2.4 Run versus AgentRun

A `Run` is the client-command execution boundary. An `AgentRun` is one attempt by an Agent to execute an Assignment inside that Run. A retry creates a new `AgentRun`; it does not create a new client Run or mutate the failed attempt.

### 2.5 Workspace versus worktree

A Workspace is authorized project scope, not a Git checkout. A worker operates through an **AssignmentWorkspaceView** backed by a shared read snapshot, a Git worktree, a copy-on-write checkout, or a remote volume. This prevents the implementation adapter from becoming domain language.

### 2.6 Message versus Event

User input, assignment briefs, inter-agent notes, progress summaries, and final authored output are Messages or typed authored records. Creation, delivery, acknowledgement, and state change are Events. Token deltas may be ephemeral stream data and do not need one durable event per token.

### 2.7 Tool, skill, plugin, and capability

- A **Tool** is an invokable operation mediated by policy.
- A **Skill** is a versioned instruction/resource package that may describe how to use tools and may bundle scripts.
- An **extension adapter** exposes out-of-process capabilities, usually through MCP.
- A **Capability** is authority to cause an effect; it is not a tool or skill declaration.

Neither a skill's `allowed-tools` metadata nor an MCP server's advertised tool list grants authority.

### 2.8 Journal immutability versus deletion and privacy

Immutable means ordinary domain history is append-only and past facts are not rewritten by a model. It does not mean user data can never be deleted. The local product default is:

- retain typed canonical events needed for active sessions and recovery;
- avoid raw prompt/tool/provider payload duplication;
- keep large or sensitive content in separately retained artifacts where possible;
- let the user delete a session/workspace history, physically remove associated content and indexes, and rebuild projections;
- record an administrative deletion receipt outside the deleted scope when policy requires it, without retaining the deleted content.

Multi-tenant/legal retention is a deferred product decision that must be resolved before a hosted service, not before the local engine.

### 2.9 “Three priority providers” versus Phase 1 scope

The studies alternately say three priority providers and “OpenAI plus one other.” The canonical rollout is fake -> OpenAI -> Anthropic for the local MVP, then Gemini to prove a third cloud shape. Ollama is orthogonal local-inference coverage. Provider-complete claims require the named conformance matrix; the first vertical slice does not.

### 2.10 MCP version assumptions

MCP is explicitly version-pinned. The 2026-07-28 release removed the protocol-level handshake/session, made requests self-describing, changed authorization, and moved Tasks to an extension ([MCP 2026-07-28 release](https://blog.modelcontextprotocol.io/posts/2026-07-28/)). This validates the adapter boundary: MCP types and lifecycle must not leak into domain state, and conformance must run for every supported dated revision.

### 2.11 Telemetry versus user-visible truth

The event journal is durable product truth. Telemetry is a sampled/exportable operational signal and may be missing. OpenTelemetry's GenAI semantic conventions are still evolving and discourage populating large optional attributes by default ([OpenTelemetry GenAI attributes](https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/)). The Engine therefore owns stable internal measurements and redaction; an OTLP adapter maps them to whichever semantic-convention version is deployed.

## 3. Researched production decision register

These decisions are the inputs the architecture diagrams should treat as settled.

### Domain and runtime

| ID | Decision |
|---|---|
| R-01 | Rust is the Engine implementation language. |
| R-02 | Domain is pure reducers, state machines, identifiers, capability semantics, and invariants. |
| R-03 | Engine owns admission, orchestration, context assembly, cancellation, recovery, budgeting, and durable transition ordering. |
| R-04 | One reusable loop executes every Agent role. Orchestrator is a role, not infrastructure. |
| R-05 | Supervision tree, assignment DAG, and event causality are separate structures. |
| R-06 | Canonical state changes become visible only after journal commit. |
| R-07 | External effects use stable invocation IDs and are idempotent or explicitly non-retryable. |
| R-08 | Every queue, buffer, artifact, body, output, concurrency pool, retry loop, and cache has a bound and overflow behavior. |

### Protocol and clients

| ID | Decision |
|---|---|
| R-09 | A language-neutral protocol becomes a public compatibility promise only when a second Client or detached execution triggers it. |
| R-10 | That triggered process seam uses versioned JSON schemas and framed JSON-RPC over protected local IPC. HTTP/SSE has a separate browser or remote trigger. |
| R-11 | Events are monotonic per aggregate; no global total order is promised. |
| R-12 | Delivery is at least once with stable IDs, idempotency, snapshots, cursors, and replay. |
| R-13 | Semantic lifecycle events are never silently dropped. Text deltas may be coalesced. Lag produces a resumable error. |
| R-14 | ACP is an IDE adapter. MCP is a driven capability adapter. A2A is a future remote-agent adapter. |
| R-15 | `arany` and conformance tests consume the same snapshots and event semantics. Any future Client must reuse this contract rather than entering the Engine. |

### Storage, context, and memory

| ID | Decision |
|---|---|
| R-16 | The demonstrator uses one SQLite Events table owned by its one process. WAL and checkpoint policy require measured concurrency or latency need. |
| R-17 | Replay is authoritative. Idempotency records, snapshots, FTS, embeddings, separate feedback views, and telemetry stores appear only with their triggering behavior. |
| R-18 | When Event payload limits are exceeded, Artifacts are content-addressed, bounded, immutable after publication, and retained separately. |
| R-19 | Context assembly has deterministic precedence, token budgets, provenance, and trust labels. Retrieved data cannot become higher-priority instructions. |
| R-20 | When retrieval is required, FTS5 comes first. Dense/vector retrieval requires a labelled quality evaluation and remains a derived index. |
| R-21 | When cross-Run Memory is required, writes are privileged, scoped, provenance-rich proposals with review, supersession, expiry, and deletion behavior. |

### Providers and tools

| ID | Decision |
|---|---|
| R-22 | Provider adapters normalize only harness semantics and preserve request IDs, usage provenance, unknown events, and bounded opaque metadata. |
| R-23 | Retries are bounded by attempt count and total deadline, honor provider hints, and never form nested hidden retry loops. |
| R-24 | A mid-stream provider failure after visible output is not automatically replayed as a fresh model call. OpenAI explicitly warns against automatic replay after streaming output, and Anthropic documents errors after an SSE response has already begun ([OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits), [Anthropic API errors](https://platform.claude.com/docs/en/api/errors)). |
| R-25 | Rate-limit admission is keyed by provider, credential/project/workspace, model, request and token dimensions, using provider headers when available. OpenAI, Anthropic, and Gemini expose different limit scopes and dimensions, so one global semaphore is insufficient ([OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits), [Anthropic rate limits](https://platform.claude.com/docs/en/api/rate-limits), [Gemini rate limits](https://ai.google.dev/gemini-api/docs/rate-limits)). |
| R-26 | When native-process Tools are triggered, they use executable-plus-argument arrays, minimal environment, validated cwd, bounded drains, process-tree supervision, cancellation, and reap. |
| R-27 | When MCP is triggered, it is version-pinned behind an adapter; remote MCP uses authenticated authorization and never forwards arbitrary upstream tokens. |
| R-28 | Any future Tool or MCP result is an untrusted observation. Advertisement and model selection never grant capability. |

### Instructions and deterministic protection

| ID | Decision |
|---|---|
| R-29 | Exact `AGENTS.md` is selected first; exact same-directory `CLAUDE.md` is absence-only fallback. |
| R-30 | Guidance, typed restrictions, compiled policy, grants, effective enforcement, and receipts are separate stages. |
| R-31 | Repository policy is restrict-only and merges by intersection with managed/user/run limits. |
| R-32 | Strict effects fail closed on parse, version, path, enforcement, or attestation uncertainty. |
| R-33 | Guard is a separate privileged process with a smaller trusted computing base than Engine. |
| R-34 | Strict no-egress is the first network guarantee. Brokered egress is a weaker, separately named profile. |
| R-35 | Jev or any classifier may advise, explain, or recommend narrowing; it cannot grant or widen authority. |

### Multi-agent and feedback

| ID | Decision |
|---|---|
| R-36 | The demonstrator success path uses one final-answer root and exactly two concurrent read-only children through the same loop. |
| R-37 | Delegation directly creates a child AgentRun. A separate Assignment appears only when dependencies, optional work, ownership transfer, or richer joins are required. |
| R-38 | The demonstrator has no automatic retry. If retries are added, each attempt receives new identity and immutable terminal history. |
| R-39 | Root success requires all children to finish successfully. Detached work does not exist in the demonstrator. |
| R-40 | Direct worker steering is deferred. If added, it identifies target and mode, is journaled, and is mirrored to the root. |
| R-41 | One RunView supplies terminal and JSONL output. Separate overview, graph, timeline, and attention projections require measured query or scale need. |
| R-42 | CLI feedback identifies whether claims are Engine facts, agent-authored summaries, or derived inferences. Hidden reasoning is never required for observability. |
| R-43 | Read-only children share explicit bounded inputs. Writers trigger isolated Workspace views and one explicit integration owner. |

### Product and operations

| ID | Decision |
|---|---|
| R-44 | The first implementation is one CLI process. A second Client or detached execution triggers a per-user daemon; remote and multi-tenant deployment is a later trust boundary. |
| R-45 | Linux uses a pathname Unix-domain socket inside a user-only runtime directory plus peer credential verification. Linux documents both pathname permissions and `SO_PEERCRED`; abstract sockets have no filesystem permissions and are therefore not the default ([Linux `unix(7)`](https://man7.org/linux/man-pages/man7/unix.7.html), [XDG Base Directory specification](https://specifications.freedesktop.org/basedir/0.8/)). |
| R-46 | Windows uses a named pipe with an explicit restrictive DACL. The platform default can grant read access to Everyone and anonymous users, so it is not acceptable unchanged ([Microsoft named-pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)). |
| R-47 | Configuration, state, cache, runtime endpoints, artifacts, and secrets have distinct storage classes. Provider secrets use OS credential storage or explicit process injection and never enter domain events. Apple's Keychain is the platform precedent for encrypted small-secret storage ([Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services)). |
| R-48 | Evaluations are harness-owned datasets, environments, budgets, traces, and graders. They do not depend on one provider's hosted eval product. OpenAI currently recommends trace grading followed by repeatable datasets, while also announcing the legacy Evals platform's 2026 shutdown ([OpenAI agent evals](https://developers.openai.com/api/docs/guides/agent-evals), [OpenAI Evals transition](https://developers.openai.com/api/docs/guides/evals)). |
| R-49 | Model selection is a versioned Engine policy over a configured model catalog, required capabilities, privacy/location constraints, user choice, evaluation evidence, and remaining budget. Provider model names and pricing never become Domain conditionals. |
| R-50 | V1 observability is an opt-in trace-only OTLP/HTTP-protobuf projection to a numeric-loopback Collector, added after the core proof. A private module owns a typed content-free allowlist, bounded batch thread, retry, and post-runtime shutdown. Export cannot change Events, RunView, cancellation, or exit status. |
| R-51 | V1 establishes authority before project input; uses private no-follow defensive SQLite with strict data-only replay and an aggregate cap; reserves one Run-owned call/token/resource budget; emits inert output; fixes and content-limits Provider egress; and reviews build/Skill provenance. It has no listener, local model-driven effect, or sandbox claim. |
| R-52 | V1 testing maximizes evidence per test. One observation bundle joins exit, exact stdout/stderr, and reopened SQLite truth; a few scenario/table/corpus/failpoint owners replace test-per-function growth, while live Provider, evaluation, performance, and cross-platform evidence remain explicit separate lanes. |
| R-53 | The product and binary are Arany and `arany`. Linux and macOS are the beta support boundary after native gates; Windows is unsupported until its deferred native suite passes. Distribution is free with attribution, but the exact license remains an explicit pre-release product decision. |
| R-54 | V1 feedback adopts the established stdout/stderr split, typed JSONL lifecycle, stable worker attribution, bounded terminal usage, structured cancellation, and actionable errors. It adds a Run ID/last-sequence receipt, exact outstanding-worker join facts, outcome/usage/recovery accounting, and a terminal ledger entry for every worker. |

## 4. Targeted research that closes previously missing modules

### 4.1 Provider runtime

The existing studies identified provider velocity as Rust's largest risk but did not fully close retry and quota ownership. The canonical provider runtime has five internal responsibilities:

1. **Capability discovery and validation:** supported modalities, tool choice, structured output, parallel tools, streaming, token counting, caching, reasoning controls, and cancellation are data, not model-name conditionals spread through Engine.
2. **Request normalization:** Engine sends a provider-neutral request plus explicit adapter extensions. Adapter DTOs never cross inward.
3. **Stream reduction:** fragmented bytes become ordered semantic provider events; unknown events are recorded safely; terminal usage may arrive late.
4. **Resilience:** per-attempt timeout, total deadline, retry classification, `Retry-After`, jitter, circuit/load state, and cancellation. Provider SDK retries must be disabled or counted inside the same budget.
5. **Evidence:** raw provider request ID, model revision, usage source, cache use, rate-limit hints, timing, and bounded diagnostic metadata.

This is necessary because each provider can fail after streaming begins. OpenAI says not to automatically replay after output has been consumed; Anthropic documents SSE errors after HTTP 200; Gemini also delivers errors as stream events ([OpenAI rate limits](https://developers.openai.com/api/docs/guides/rate-limits), [Anthropic errors](https://platform.claude.com/docs/en/api/errors), [Gemini API errors](https://ai.google.dev/gemini-api/docs/api-errors)). Therefore the journal must distinguish:

- rejected before any provider-visible output;
- retryable attempt before output;
- interrupted after partial output;
- provider completed but local acknowledgement was uncertain;
- tool side effect completed while the provider turn later failed.

A fresh inference after partial output is an explicit new attempt with causation, not a transparent transport retry.

### 4.2 Configuration, identity, and secrets

Configuration is a composition-root concern, not a Domain service. Use separate inputs:

- compiled defaults;
- managed/admin constraints;
- user configuration;
- workspace configuration in a closed, non-secret namespace;
- invocation flags for non-security preferences;
- explicit runtime/session commands.

Security constraints combine monotonically rather than ordinary last-writer-wins precedence. Repository configuration cannot select host secret values, add unrestricted executable paths, weaken retention, or widen capabilities.

On Linux, use the XDG distinction between configuration, state, data, cache, and runtime endpoints; the runtime directory is specifically intended for sockets and must be user-only ([XDG Base Directory specification](https://specifications.freedesktop.org/basedir/0.8/)). Equivalent platform directory APIs are adapters. Secret configuration stores only provider/credential handles. The adapter redeems a handle as late as possible and receives a minimally scoped secret; domain events, errors, metrics, crash reports, and child environments never receive the raw value.

### 4.3 Triggered daemon lifecycle and local trust

The demonstrator has no daemon. If a second Client or detached execution triggers one, the per-user daemon becomes the single owner of SQLite, leases, provider admission, Tool processes, and live subscriptions. Clients may auto-start it using an OS-specific lock/activation mechanism, then connect to a protected endpoint. The protocol begins only after:

- peer identity is checked;
- protocol version and client capabilities are negotiated;
- maximum frame/body limits are installed;
- principal and Workspace scope are derived from trusted state, not accepted from a client claim.

The demonstrator exposes no browser listener and no HTTP/SSE transport. Browser origin controls, session tokens, remote binding, OAuth, TLS termination, CSRF, tenancy, and organization Policy remain deferred until a named non-CLI or remote product requirement creates that trust model.

### 4.4 Evaluation subsystem

Evaluation is not only a CI test runner. It is a replaceable adapter around a stable harness-owned experiment record:

```text
EvaluationCase
  input + initial workspace/state + allowed tools + expected invariants

EvaluationRun
  exact harness build + model/provider + prompts + budgets + seeds where available
  event range + artifacts + outcome + usage + timing

EvaluationResult
  deterministic checks + human labels + model graders with provenance
```

Use separate suites for:

- deterministic state/replay/security correctness;
- provider and protocol conformance;
- task capability and answer quality;
- single-agent versus multi-agent comparison;
- safeguard/adversarial robustness;
- user legibility and accessibility;
- latency, resource use, and cost.

Every score names the claim it supports, task distribution, harness version, tools, budgets, scorer, and validity threats. OpenAI's current agent-evaluation guidance recommends workflow traces for discovering behavior failures and repeatable datasets and eval runs for comparisons and regressions ([OpenAI agent evals](https://developers.openai.com/api/docs/guides/agent-evals)). That is the correct model for this project: never publish “multi-agent is better” without a same-budget baseline and decomposition-sensitive task set.

### 4.5 Observability and diagnostics

The canonical event journal, user timeline, protected diagnostic log, metrics, and distributed trace are five different products:

| Surface | Authority | Default content |
|---|---|---|
| Journal | Canonical | Typed state transitions and authored records needed for recovery |
| User timeline | Derived product view | Safe facts, authored summaries, attention, evidence links |
| Diagnostic log | Operational | Reason codes, IDs, sizes, safe errors; content excluded by default |
| Metrics | Aggregate | Latency, queues, bytes, counts, cost/usage provenance |
| Trace | Causal operational view | Span boundaries and identifiers; prompts/tool content opt-in only |

For the minimum CLI, one private `telemetry.rs` module accepts typed safe operational facts and contains every OpenTelemetry type. It is not a trait or public facade. The successful fixed workflow produces exactly nine spans; trace export is disabled without an endpoint and can only target a numeric-loopback Collector over OTLP/HTTP binary protobuf. Prompts, objectives, instructions, summaries, results, paths, file metadata or contents, Event payloads, provider bodies, headers, credentials, arbitrary attributes, and formatted errors are forbidden. The local Collector owns remote TLS, authentication, routing, and vendor configuration.

The broader production design may eventually earn a versioned internal telemetry facade, but only when a second implementation or independently owned consumer exists. OTel remains an exporter mapping rather than a domain type. The focused [OTLP observability report](./otlp-observability.md) owns exact configuration precedence, queue/request/retry/shutdown limits, semantic mapping, dependency features, and executable gates.

### 4.6 Skills and extension lifecycle

The Agent Skills format standardizes a folder with `SKILL.md` metadata/instructions and optional scripts, references, and assets ([Agent Skills specification](https://agentskills.io/specification), [official Agent Skills repository](https://github.com/agentskills/agentskills)). It does not turn the package into trusted code and does not replace capability policy.

Canonical lifecycle:

1. Install into a content-addressed store from an explicit source.
2. Validate format, file count, sizes, paths, symlinks, and supported metadata.
3. Record source, resolved revision when available, tree/content digest, license metadata, and install time in the lockfile.
4. Index only bounded metadata for discovery.
5. Load the selected `SKILL.md` as attributed untrusted guidance through the context compiler.
6. Resolve bundled files only under the pinned skill root.
7. Treat bundled scripts as tool requests requiring ordinary capability decisions and sandbox execution.
8. Updates create a new immutable version and require an explicit diff/approval policy. A lockfile hash mismatch fails closed.

`allowed-tools` is advisory compatibility metadata. It may request or narrow a tool set but cannot authorize one. Signature/transparency verification can be added for a curated marketplace; Sigstore supports identity-bound artifact and blob verification with digest checking ([Sigstore verification](https://docs.sigstore.dev/cosign/verifying/verify/)). Marketplace distribution and automatic updating are deferred product scope, not an MVP dependency.

### 4.7 Release, migration, backup, and update boundary

Schema migration belongs beside the storage/protocol adapter, never inside reducers as ad hoc SQL conditionals. Every release declares:

- readable event schema versions and deterministic upcasters;
- snapshot invalidation/rebuild rules;
- protocol compatibility window and generated golden fixtures;
- database migration preconditions, rollback/backup procedure, and free-space bound;
- artifact garbage-collection and retention compatibility;
- minimum guard/backend capability versions.

The local MVP supports explicit user-initiated upgrades and documented backup/restore. Silent self-update is deferred. When an updater exists, it must verify signed metadata/artifacts before replacement and preserve rollback protection; this is a new security review boundary, not a helper hidden inside Engine.

### 4.8 Workspace integration and code intelligence

Version control is an adapter, not the Workspace definition. The first code-writing slice needs:

- immutable base revision/snapshot identity;
- isolated writer view;
- declared write set or reservation hints;
- deterministic patch/change-set artifact;
- verification receipts;
- one integration owner;
- conflict and stale-base detection;
- cleanup after cancellation.

Git worktrees are the first adapter because they preserve ordinary Git semantics and match the visible-worker precedent. Large repository, submodule, ignored-file, LFS, sparse checkout, nested repository, case-folding, and Windows path behavior remain implementation fixtures. Language-server integration is a Tool or code-intelligence adapter, not part of Domain or the agent loop.

### 4.9 Model catalog, routing, usage, and cost

Model identity is not a free-form string scattered through prompts or agent definitions. A provider adapter contributes observed capabilities and provider identifiers to a versioned catalog entry; trusted user/admin configuration may add aliases, availability, residency/privacy constraints, allowed roles, and budget policy. An `Agent` requests capabilities and optionally a preferred model class. Engine policy resolves that request to an exact provider/model revision and records the decision.

The demonstrator does not need an autonomous model router. It uses one explicit model setting. Later routing can optimize quality, latency, or cost only after evaluation data supports the choice; Jev or another classifier may recommend a route but cannot bypass privacy, capability, or spend constraints.

Usage values retain provenance: provider-reported, locally counted, estimated, or unknown. Prices live in a dated adapter/configuration catalog because they change independently from Engine releases. A cost ledger stores input, cached input, output, reasoning, tool/provider-specific units, currency, price-card version, and estimate/final status. Budgets reserve before admission where possible, reconcile after provider usage arrives, and fail visibly when exact cost is unavailable. This closes model routing and cost attribution as Engine policy plus provider metadata rather than a new external service.

## 5. Closure register for every remaining open item

| Item | Status | Evidence needed or trigger |
|---|---|---|
| Rust owned-path performance | Implementation gate | Reference host, release builds, fake provider, direct baseline, p50/p95/p99, CPU/RSS, overload. |
| Go/TypeScript comparison | Implementation gate | Same semantics and fixtures; used to quantify maintenance/runtime cost, not reopen Rust by default. |
| OpenAI/Anthropic/Gemini capability parity | Implementation gate | Recorded fixtures and opt-in live contract suite per model/API revision. |
| Ollama/local inference | Deferred product decision | Triggered by a named offline/local-model requirement. |
| ACP adoption depth | Implementation gate | Map prompt, updates, approval, cancel, artifacts, replay, multi-client, and extensions. Keep as adapter regardless. |
| C++ CLI | Deferred product decision | Triggered by a measured UX/ecosystem/ownership requirement that Rust/TS cannot meet. |
| Additional web, desktop, IDE, or standalone SDK Client | Deferred product decision | Triggered by a named product workflow that the CLI cannot satisfy; must reuse the semantic protocol. |
| HTTP/SSE transport | Deferred product decision | Triggered by a browser, remote, or multi-client requirement and accompanied by its authentication and threat model. |
| FFI | Deferred product decision | Triggered only by an embedding requirement where a daemon is unacceptable. |
| Vector database | Deferred product decision | Labelled retrieval eval proves FTS/hybrid quality gap and operating benefit. |
| PostgreSQL | Deferred product decision | Multiple authoritative writers/hosts, tenancy, replication, backup, or SQLite SLO failure. |
| WASI plugins | Deferred product decision | Stable component capability API, toolchain maturity, and security review. |
| Remote gateway/multi-tenancy | Deferred product decision | Hosted/remote product requirement with identity, authorization, encryption, retention, quota, and operations design. |
| macOS strict native protection | Implementation gate | Same adversarial conformance suite and supportable public mechanism. |
| Windows strict native protection | Implementation gate | Same suite across LPAC/AppContainer and dedicated-principal/WFP options. |
| Higher-assurance VM/gVisor tier | Deferred product decision | Hostile/multi-tenant workload or native-backend insufficiency. |
| Brokered egress | Implementation gate | SSRF/DNS/redirect/TLS/proxy/private-network corpus and residual-trust documentation. |
| Resource-profile defaults | Implementation gate | Representative build/tool workloads and failure-legibility testing. |
| Codex/Claude/OpenCode instruction compatibility | Deferred product decision | Named compatibility need, pinned product version, and fixtures. |
| Instruction imports | Deferred product decision | Beta has no imports. Add bounded guidance-only relative imports only after a named repository need and adversarial resolver fixtures; never remote imports. |
| Nested policy with broad process access | Implementation gate | Eager subtree discovery or exact resource grants proven by path/enforcement tests. |
| Three-worker worktree strategy | Implementation gate | Creation latency, disk, large repos, conflicts, cleanup, and platform fixtures. |
| Peer agent collaboration | Deferred product decision | Named eval outperforms manager pattern under equal budget and stays legible. |
| Handoffs and nested supervisors | Deferred product decision | Scenario requires ownership transfer or deeper hierarchy and passes recovery/UX tests. |
| Detached child work | Deferred product decision | Separate background-run product with ownership, notification, budget, retention, and cancellation. |
| A2A remote workers | Deferred product decision | Remote-agent product and trust model. |
| Generated progress summaries | Resolved optional feature | Label as derived, non-authoritative, bounded, and never critical path. |
| Transcript retention for hosted use | Deferred product decision | Product/legal/tenant policy before remote launch. Local defaults are defined above. |
| V1 OTLP trace export | Resolved optional second patch plus implementation gates | Trace-only HTTP/protobuf to numeric loopback, pinned `0.33.0` mapping, privacy/protobuf/lifecycle/performance tests. |
| Direct remote OTLP, logs, and metrics | Deferred product decisions | Remote trust requirement, concrete noncanonical diagnostic need, or daemon/aggregate-SLO trigger. |
| Hosted vendor eval platform | Resolved non-dependency | Harness-owned datasets/traces/graders; vendor integrations are adapters. |
| Skill marketplace/signing | Deferred product decision | Curated remote distribution or auto-update requirement. |
| Silent self-update | Deferred product decision | Signed update design, rollback protection, recovery, and user policy. |
| V1 security claim and P0 controls | Resolved decisions plus release gates | [Harness security lessons](./harness-security-lessons-and-controls.md), D-14, and the incident-derived cross-platform suites. |
| V1 test ownership and exact user-visible proof | Resolved decision plus release gates | [Testing strategy](./testing-strategy-for-rust-cli-harness.md), D-15, and the observation-bundle/platform suites. |
| Product identity and beta platforms | Resolved decision plus release gates | Arany/`arany`; native Linux and macOS suites block beta support; Windows is deferred. |
| Exact open-source license | Deferred product decision before public release | Select and add the legal instrument that implements free use plus author attribution; do not infer a grant from prose. |
| V1 CLI feedback pattern | Resolved decision plus executable proof | [Cross-harness comparison](./cli-user-feedback-patterns-from-agent-harnesses.md), D-08, exact-output goldens, replay parity, and stream-truncation recovery. |

There are no remaining **Missing research** items required to begin Phase 0. The deferred rows have explicit triggers; the gate rows require executable evidence. If a trigger becomes part of the initial product scope, it creates a new focused research/ADR task rather than reopening the whole architecture.

## 6. Missing-module audit

The following table answers “which harness modules have not been analyzed?” after incorporating the targeted work above.

| Module or responsibility | Coverage after this audit | Next authoritative artifact |
|---|---|---|
| Domain vocabulary and reducers | Deep | Phase 0 ADR and state-machine fixtures |
| Engine lifecycle and agent loop | Deep | Phase 0 interface/state ADR |
| Multi-agent supervisor/scheduler | Deep architecture; unproved | Phase 2/3 implementation plan and chaos/eval results |
| Feedback projections and CLI UX | Deep architecture and cross-harness pattern review; unproved | D-08 exact terminal/JSONL/replay evidence, stream receipt, join causality, and partial-result ledger |
| Process protocol | Researched; deferred | Triggered by a second Client or detached execution, then schema and local-IPC conformance |
| Local daemon and authentication | Researched; deferred | Same trigger, then lifecycle/security ADR and platform tests |
| Provider runtime | Deep architecture; adapters unproved | Provider port ADR and conformance corpus |
| Model catalog/routing/cost ledger | Architecture closed here; policy unproved | Catalog schema, usage normalization, dated price cards, and routing evals |
| Context compiler | Deep architecture; implementation unproved | Prompt-plan schema, token-budget fixtures, model-specific adapters |
| Journal/snapshots/projections | Deep baseline | Storage transaction/migration ADR and fault suite |
| Long-term memory/retrieval | Deep architecture; quality unproved | Memory schema and labelled eval corpus |
| Artifact store | Architectural baseline | Retention/atomic-publication/GC design and fault tests |
| Tool runtime/process supervisor | Deep architecture; unproved | Tool contract and process-tree conformance suite |
| MCP adapter | Baseline plus current version warning | Pinned revision, auth design, official conformance suite |
| Deterministic policy and guard | Deep | Policy/guard protocol ADR and adversary corpus |
| Instruction resolver | Deep | Resolver/policy schemas and filesystem adversarial tests |
| Configuration and composition | Architecture closed here | Typed configuration schema and precedence/security tests |
| Identity/secrets | Local baseline closed here | Platform secret-store and local-principal adapters |
| Skills lifecycle | Architecture closed here | Skill manifest/lock schema and install/security tests |
| Workspace/VCS integration | Architecture closed here | Workspace adapter contract and Git fixture suite |
| Telemetry/diagnostics | Baseline closed here | Redaction schema and exporter mapping tests |
| Evaluation framework | Deep architecture; datasets unimplemented | Dataset/trace/result schemas and first baseline suite |
| Testing and release proof | V1 contract resolved; executable evidence pending | D-15 scenario/table/corpus owners and native platform gates |
| Packaging/migrations/backup/update | Baseline closed here; updater deferred | Release/migration plan before distributable MVP |
| Remote gateway/multi-tenancy | Intentionally deferred | New threat model and research when triggered |
| Marketplace/native/WASI plugins | Intentionally deferred | New supply-chain/capability research when triggered |

This is enough coverage to build an architecture diagram without inventing unnamed boxes. It is not a recommendation to create one crate per row. Most rows begin as private modules inside Engine or adapter crates and become physical boundaries only when the repository's crate test is satisfied.

## 7. Evidence required to call the research phase complete

The research phase is complete only when every item below has authoritative evidence.

### 7.1 Inventory and source quality

- Every research document is inventoried with its decisions and gates.
- Literal priority/placeholder markers are searched across research, planning, and context.
- Factual ecosystem claims cite current first-party documentation, specifications, source code, or original papers.
- Version-sensitive claims name a date, version, revision, or implementation gate.
- Recommendations are distinguishable from observed facts.

**Evidence in this repository:** the subject-document inventories above, the closure register, and inline primary sources. The repository search found no P-number markers and only the intentional plan-template placeholder.

### 7.2 Decision completeness

- Every architecture-driving question is either a resolved decision, a deferred product decision with a trigger, or an implementation gate with a measurement.
- No “maybe,” “later,” “TBD,” or “choose A/B” remains in the architecture path without one of those statuses.
- Provider, protocol, storage, policy, instructions, agent loop, feedback, configuration, secrets, skills, evaluation, and operations have an owner and dependency direction.

**Evidence in this repository:** sections 1, 3, 5, and 6 of this audit.

### 7.3 Contradiction closure

- Engine versus orchestrator authority is unambiguous.
- Journal, Memory, Context, Artifact, telemetry, Message, and Event are not conflated.
- Run and AgentRun nesting is explicit.
- Workspace does not mean worktree.
- Tool, Skill, extension, and Capability are distinct.
- Retention/deletion does not create a false “immutable forever” guarantee.
- Provider and MCP version assumptions are explicit.

**Evidence in this repository:** section 2 of this audit. `CONTEXT.md` should be updated only when the Phase 0 vocabulary ADR is accepted, as the domain-modeling workflow requires.

### 7.4 Diagram readiness

The system is ready for diagrams when the diagrams can show, without unresolved unnamed arrows:

1. dependency direction;
2. runtime/process boundaries and trust zones;
3. command, snapshot, event, and artifact flow;
4. one agent-loop step with durable/effect boundaries;
5. supervision tree versus assignment DAG;
6. capability request -> policy -> grant -> guard -> attestation;
7. instruction discovery -> guidance/policy split;
8. journal -> reducers -> projections -> clients;
9. provider/tool waits and cancellation propagation;
10. local deployment now and deferred remote boundaries later.

**Evidence in this repository:** the canonical decision register supplies each node and arrow.

### 7.5 What research cannot prove

The following claims remain false until their executable gates pass:

- “Harness overhead is negligible.”
- “Acknowledged events are never lost.”
- “Replay always reconstructs identical state.”
- “No database access is impossible under the strict profile.”
- “The same strict profile works on Linux, macOS, and Windows.”
- “All provider adapters behave equivalently.”
- “Three agents are faster or better than one.”
- “Worktrees are cheap enough for every repository.”
- “The CLI always tells the user what needs attention.”
- “No sensitive content reaches logs, traces, or clients.”

These statements belong in exit criteria and test reports, not in architecture prose.

## 8. Recommended implementation-research handoff

The research phase hands off exactly one vertical proof:

1. Write the implementation plan with the V1 security claim/non-claim, P0 controls, and supported-platform evidence.
2. Establish trusted startup ordering, private state-root admission, bounded Workspace snapshots, and inert output before any live Provider call.
3. Persist the five required Event kinds in hardened SQLite and prove strict data-only replay, crash/disk/corruption handling, and aggregate growth admission.
4. Drive a root plus two read-only child AgentRuns through one scripted Provider test under one reserved aggregate Run budget, then prove cascading cancellation and backpressure cleanup.
5. Add OpenAI as the second Provider adapter, pass hostile-network and secret/egress-canary tests, and run the same scenario live.

No ADR, daemon, process protocol, Guard, Tool runtime, Memory store, Artifact store, Assignment DAG, snapshot, search index, or evaluation subsystem precedes this proof. Each later slice carries its trigger and implementation gate from the register.

## 9. Final conclusion

The remaining uncertainty is honest and bounded. There is one minimum implementable architecture:

- one Rust package and CLI process;
- one deep Engine module and one reusable agent loop;
- one Provider interface with scripted fake and OpenAI adapters;
- one Run-owned AgentRun tree with exactly two read-only children in the success path;
- one SQLite Events table and one replayed RunView; and
- explicit bounded input files, inert terminal/JSONL feedback, aggregate budgets, fixed content-limited Provider egress, and cascading cancellation.

The broader research remains available when a measured trigger fires; it does not create initial modules. There is no P-number bucket left whose meaning must be guessed.

## Primary sources added by this closure audit

- [OpenAI API rate limits and retry guidance](https://developers.openai.com/api/docs/guides/rate-limits)
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors)
- [Anthropic API rate limits](https://platform.claude.com/docs/en/api/rate-limits)
- [Gemini API errors](https://ai.google.dev/gemini-api/docs/api-errors)
- [Gemini API rate limits](https://ai.google.dev/gemini-api/docs/rate-limits)
- [MCP 2026-07-28 release](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [OpenTelemetry GenAI semantic-convention attributes](https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/)
- [OpenAI agent workflow evaluation guidance](https://developers.openai.com/api/docs/guides/agent-evals)
- [OpenAI Evals transition notice](https://developers.openai.com/api/docs/guides/evals)
- [Agent Skills specification](https://agentskills.io/specification)
- [Official Agent Skills repository](https://github.com/agentskills/agentskills)
- [Sigstore artifact verification](https://docs.sigstore.dev/cosign/verifying/verify/)
- [Linux Unix-domain sockets and peer credentials](https://man7.org/linux/man-pages/man7/unix.7.html)
- [XDG Base Directory specification](https://specifications.freedesktop.org/basedir/0.8/)
- [Microsoft named-pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)
- [Apple Keychain Services](https://developer.apple.com/documentation/security/keychain-services)
- [TypeSafe Jev introduction](https://docs.typesafe.ai/introduction)

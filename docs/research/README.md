# Arany research index

**Status:** implementation decisions complete; executable gates pending  
**Decision register:** [next-step-decision-register.md](./next-step-decision-register.md)  
**Closure audit:** [research-closure-audit.md](./research-closure-audit.md)  
**Human review map:** [Arany conceptual overview](../architecture/arany-conceptual-overview.md)  
**Canonical beta architecture:** [system-overview.md](../architecture/system-overview.md)

The earlier conversational `P0`/`P1`/`P2` shorthand is retired. It mixed research questions, implementation proof, and intentionally deferred scope. Every remaining item now has one explicit status:

- **Resolved decision:** the architecture has a canonical direction.
- **Implementation gate:** a spike, test, benchmark, evaluation, or runtime check must prove a product claim.
- **Deferred product decision:** the feature is outside the minimum demonstrator and has a named trigger.
- **Missing research:** none required for Phase 0.

## Subject reports

| Report | Primary decision area | Status |
|---|---|---|
| [Rust core feasibility](./rust-core-feasibility.md) | Engine language, storage, latency, async runtime, process protocol | Rust selected; its broader WAL/FTS/process shape is superseded for the minimum slice |
| [Agent-state persistence](./agent-state-persistence.md) | SQLite versus files, Rust KV stores, RocksDB, and PostgreSQL; crash/durability contract | SQLite `DELETE + EXTRA`, one strict Events table, commit-before-feedback |
| [Rust foundation and Engine contract](./rust-foundation-and-engine-contract.md) | Exact runtime, dependencies, Provider/Engine/Event types, store thread, HTTP path | Current-thread Tokio, dedicated SQLite thread, four-call workflow, bounded non-streaming OpenAI |
| [CLI inputs and operability](./cli-inputs-configuration-and-operability.md) | Commands, inputs, limits, output, configuration, credentials, cancellation, fixtures | Complete fixed V1 CLI and evidence contract |
| [Testing strategy](./testing-strategy-for-rust-cli-harness.md) | Evidence-dense Rust/CLI testing, exact user output, SQLite replay, live Provider smoke, flake and deletion policy | V1 testing contract resolved; executable proof pending |
| [Existing-harness CLI feedback patterns](./cli-user-feedback-patterns-from-agent-harnesses.md) | Primary-source comparison of progress, attribution, approvals, replay, machine output, and failure UX | Reuse/adapt/reject guidance for Arany's append-only beta |
| [Modular harness architecture](./modular-harness-architecture.md) | Deep modules, dependency direction, protocol/client seams, polyglot boundary | Research retained; first physical shape reduced to one package and one Provider seam |
| [Deterministic protection](./deterministic-harness-protection.md) | Capabilities, Policy, Guard, platform enforcement, Jev boundary | Required before the first effectful Tool; absent from the read-only proof |
| [Harness security lessons and controls](./harness-security-lessons-and-controls.md) | Primary-source failures in coding harnesses; V1 threat model, release blockers, non-claims, and future effect gates | P0 controls apply before implementation; separate Guard still activates with the first effectful Tool |
| [Instruction Markdown](./instruction-markdown-and-policy-enforcement.md) | `AGENTS.md`, `CLAUDE.md`, imports, typed restrict-only Policy | Root `AGENTS.md` and absence-only `CLAUDE.md` fallback included; expansion deferred |
| [Multi-agent loop and feedback](./multi-agent-loop-and-user-feedback.md) | Reusable loop, supervision, Assignment DAG, scheduling, user feedback | Same loop retained; reduced to one root, two children, join-all, and one RunView |
| [Context, Memory, and compaction](./context-memory-and-compaction.md) | Context compiler, Memory lifecycle, retrieval, caching, deletion | Bounded input remains private Engine implementation; cross-Run Memory is deferred |
| [Provider and tool runtime](./provider-and-tool-runtime.md) | Provider adapters, effect coordinator, processes, MCP, PTY/jobs | Fake and OpenAI retained; effect runtime, MCP, and PTY are deferred |
| [Evaluation and quality](./evaluation-and-quality-strategy.md) | Cases, trials, graders, baselines, release evidence | One deterministic architecture test retained; evaluation runtime is deferred |
| [OTLP observability](./otlp-observability.md) | Standard trace export, privacy, local Collector trust boundary, lifecycle, bounds, and evidence gates | Opt-in trace-only second V1 patch after the core proof; Events remain canonical |
| [Research closure audit](./research-closure-audit.md) | Conflicts, 54-decision register, remaining gates and deferred scope | Canonical research closure |
| [Next-step decision register](./next-step-decision-register.md) | Reconciliation of every immediate next step and future trigger | Canonical implementation-research handoff |

## Canonical minimum demonstrator

The [architecture overview](../architecture/system-overview.md) now applies the Ponytail deletion test to the research. The first implementation contains only:

- one Cargo package and one CLI process;
- one deep Engine module;
- one fixed four-call workflow for a root orchestrator and exactly two read-only children;
- one Provider interface with a deterministic fake and OpenAI;
- exact root `AGENTS.md`, with exact root `CLAUDE.md` as absence-only fallback;
- one SQLite Events table owned by a dedicated thread using rollback `DELETE + EXTRA`;
- one replayed RunView rendered as append-only terminal output or JSONL; and
- one evidence-dense deterministic team journey, compact boundary corpora, exact user-output goldens, real `show` subprocess proof, and one ignored live OpenAI smoke; then
- one private, opt-in OTLP/HTTP-protobuf trace projection to a numeric-loopback Collector, with no prompt or repository content.

Before those behaviors run, startup pins trusted CLI-selected Workspace and state roots without reading repository/Git configuration, `.env`, hooks, or plugins. SQLite opens no-follow in a private owner-only state directory with defensive replay and an aggregate cap; one Run-owned budget bounds every child; terminal output is inert; and Provider egress is fixed and content-allowlisted. The demonstrator has no effectful Tool and therefore makes no sandbox claim. The first effectful Tool activates immutable typed effects, deterministic Policy, exact approval binding, and the separate Guard before that Tool ships.

The subject reports remain design evidence, not a scaffold list. Daemon, protocol, Session, Assignment DAG, Memory, Artifacts, snapshots, FTS5, additional Providers, MCP, PTY, worktrees, evaluation runtime, HTTP/SSE, other Clients, direct remote telemetry, OTLP logs, and OTLP metrics are all trigger-based future scope.

## Handoff

Research claims and implementation decisions are complete enough to write the first implementation plan. That plan must import the V1 claim/non-claim and P0 controls from the security report plus the evidence owners and admission rule from the testing report before code starts. Product guarantees are intentionally not claimed until the corresponding gates in the decision register, subject reports, and architecture document pass.

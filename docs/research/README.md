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
| [Agent instructions and product specifications](./agent-instructions-and-product-specs-2026-10.md) | Conditional instruction loading, focused behavior specs, ADRs, Skills and evidence ownership | Adopted incremental split; [terminal spec](../specs/terminal.md), documentation map and scenario-first workflow implemented; runtime discovery unchanged |
| [Clean Architecture principles](./clean-architecture-principles-2026-10.md) | Martin's dependency rule and SOLID, interpreted against Arany's real ownership and privilege seams | Three-reviewer application and dispositions in the [architecture review](../architecture/clean-architecture-review-2026-10.md) |
| [Current harness effect loops](./effectful-harness-tool-loops-2026-10.md) | Codex, Claude Code, Pi and OpenCode proposal/observation/recovery ownership | Implemented strict semantic primary continuation; native function-call migration is not required for the local base |
| [MCP and runtime Skills](./mcp-and-runtime-skills-2026-10.md) | Current MCP versions, complete legacy tools flow, portable progressive Skills, bounded parser choices | Implemented pinned local 2025-11-25 stdio profile and progressive resources; newer stateless/remote profiles remain separate |
| [Platform effect Guard](./effect-guard-platform-base-2026-10.md) | Kernel filesystem/namespace/cgroup enforcement, lifecycle, quotas and immutable receipts | Implemented native Linux local Guard; native macOS and alternative enforcers remain future adapters |
| [Rust core feasibility](./rust-core-feasibility.md) | Engine language, storage, latency, async runtime, process protocol | Rust selected; its broader WAL/FTS/process shape is superseded for the minimum slice |
| [Agent-state persistence](./agent-state-persistence.md) | SQLite versus files, Rust KV stores, RocksDB, and PostgreSQL; crash/durability contract | SQLite `DELETE + EXTRA`, one strict Events table, commit-before-feedback |
| [Rust foundation and Engine contract](./rust-foundation-and-engine-contract.md) | Exact runtime, dependencies, Provider/Engine/Event types, store thread, HTTP path | Runtime/storage/network baseline retained; fixed topology is superseded by the Session/team report |
| [CLI inputs and operability](./cli-inputs-configuration-and-operability.md) | Commands, inputs, limits, output, configuration, credentials, cancellation, fixtures | Input/security baseline retained; Session/team/provider surface superseded by the focused report |
| [Testing strategy](./testing-strategy-for-rust-cli-harness.md) | Evidence-dense Rust/CLI testing, exact user output, SQLite replay, live Provider smoke, flake and deletion policy | Evidence discipline retained; central scenario expanded to Session + bounded N |
| [Existing-harness CLI feedback patterns](./cli-user-feedback-patterns-from-agent-harnesses.md) | Primary-source comparison of progress, attribution, approvals, replay, machine output, and failure UX | Comparative evidence retained; focused terminal decisions are superseded by the beta terminal report |
| [Beta terminal interface and multi-agent feedback](./beta-terminal-interface-and-multi-agent-feedback.md) | Attached-human control room, deterministic automation, accessibility, terminal lifecycle, and PTY evidence | Native scrollback/lifecycle retained; default layout and transient mouse superseded by the Session/team report |
| [Interactive CLI conventions and command surface](./interactive-cli-conventions-and-command-surface.md) | Entry-point conventions, startup flags, slash commands, parsing, lifecycle, and permission controls | Bare `arany` and parsing retained; one-Run/session deferral is superseded |
| [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) | Footer/activity UX, mouse, bounded N, durable Sessions, custom endpoints, and attribution license | Canonical product decisions: Session beta, `/agents`, verified custom profiles, Apache-2.0 + NOTICE |
| [Modular harness architecture](./modular-harness-architecture.md) | Deep modules, dependency direction, protocol/client seams, polyglot boundary | Research retained; first physical shape reduced to one package and one Provider seam |
| [Deterministic protection](./deterministic-harness-protection.md) | Capabilities, Policy, Guard, platform enforcement, Jev boundary | Required before the first effectful Tool; absent from the read-only proof |
| [Harness security lessons and controls](./harness-security-lessons-and-controls.md) | Primary-source failures in coding harnesses; V1 threat model, release blockers, non-claims, and future effect gates | P0 controls apply before implementation; separate Guard still activates with the first effectful Tool |
| [Instruction Markdown](./instruction-markdown-and-policy-enforcement.md) | `AGENTS.md`, `CLAUDE.md`, imports, typed restrict-only Policy | Root `AGENTS.md` and absence-only `CLAUDE.md` fallback included; expansion deferred |
| [Multi-agent loop and feedback](./multi-agent-loop-and-user-feedback.md) | Reusable loop, supervision, Assignment DAG, scheduling, user feedback | Same loop retained; beta now uses one primary plus bounded `0..N` direct children |
| [Context, Memory, and compaction](./context-memory-and-compaction.md) | Context compiler, Session/Memory lifecycle, retrieval, caching, deletion | Session context and compaction activated; cross-Session Memory remains deferred |
| [Provider and tool runtime](./provider-and-tool-runtime.md) | Provider adapters, effect coordinator, processes, MCP, PTY/jobs | General evidence retained; the local effect base is now defined by the October reports; interactive/background Tool jobs remain deferred |
| [Beta multi-provider routing and adapters](./beta-multi-provider-routing-and-adapters.md) | OpenAI, Anthropic, OpenRouter, and Z.AI capability, routing, privacy, and strict-output evidence | Native OpenAI + Anthropic retained; built-in OpenRouter deferred, exact custom profiles added by the focused report |
| [Consumer subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) | OpenCode/Codex/Claude subscription flows, official policy, OAuth, secret lifecycle, and provider conformance | Historical baseline; current beta 1 scope and provider policy are updated below |
| [October 2026 subscription route recheck](./subscription-provider-routes-2026-10.md) | Official ChatGPT open-source plan route and Anthropic third-party subscription restrictions | ChatGPT is beta 1 priority; Anthropic native plan access requires prior approval or a new published route |
| [Pi Anthropic subscription route check](./pi-anthropic-subscription-2026-10.md) | Pi's native OAuth implementation versus Anthropic's third-party authorization and billing boundary | Pi's technical route is not an authorization for Arany; retain API-key support pending approval or a published route |
| [Evaluation and quality](./evaluation-and-quality-strategy.md) | Cases, trials, graders, baselines, release evidence | One deterministic architecture test retained; evaluation runtime is deferred |
| [OTLP observability](./otlp-observability.md) | Standard trace export, privacy, local Collector trust boundary, lifecycle, bounds, and evidence gates | Opt-in trace-only final beta slice; dynamic Session/Run/Agent topology, Events remain canonical |
| [Research closure audit](./research-closure-audit.md) | Conflicts, decision register, remaining gates and deferred scope | Canonical research closure; terminal/provider amendments apply |
| [Next-step decision register](./next-step-decision-register.md) | Reconciliation of every immediate next step and future trigger | Canonical implementation-research handoff |

## Canonical beta

The [architecture overview](../architecture/system-overview.md) now applies the Ponytail deletion test to the research. The first implementation contains only:

- one Cargo package and one CLI process;
- one deep Engine plus focused Session/context implementation;
- durable multi-Run Sessions with explicit resume, fork, rename, and derived compaction;
- one primary plus ordered budget-bounded `0..N` direct read-only children under `single`, `auto`, or `team` policy;
- one Provider interface with a deterministic fake, native OpenAI and Anthropic, and exact data-free-conformance-qualified custom endpoint/model profiles;
- exact root `AGENTS.md`, with exact root `CLAUDE.md` as absence-only fallback;
- one SQLite Events table owned by a dedicated thread using rollback `DELETE + EXTRA`;
- replayed SessionView/RunView projected through bare interactive `arany`, accessible linear mode, deterministic text/JSONL `exec`, and deterministic `show`;
- a primary-screen transcript with fixed-bottom composer/status, keyboard-complete pickers, transient picker-only mouse, and a closed trusted slash registry;
- current API-key authentication plus a separately consented ChatGPT-plan route under development, Apache-2.0 + NOTICE distribution policy, and one evidence-dense deterministic Session/team journey;
- one private, runtime-opt-in OTLP/HTTP-protobuf trace projection with dynamic bounded agent topology and no prompt or repository content; and
- opt-in primary-only typed local Tools through one private Policy/router and separately enforcing same-binary Guard; bounded native file operations, offline commands, pinned progressive Skills and exact-version stdio MCP.

Before those behaviors run, startup pins trusted CLI-selected Workspace, configuration, ProviderProfile, and state roots without reading repository/Git configuration, `.env`, hooks, or plugins. SQLite opens no-follow in private state with defensive replay and an aggregate cap; Run budgets bound every child and Tool; terminal output is inert; and Provider egress is admitted and content-allowlisted. Default Runs remain read-only. The [local coding base](../../planning/effectful-beta-base/README.md) activates typed one-use intents, restrict-only private grants and the native Guard rather than assuming prompts or process groups enforce permission. Its synthetic Linux proof is not live-model or cross-platform proof.

The subject reports remain design evidence, not a scaffold list. Daemon, other Clients, nested teams, Assignment DAG, cross-Session Memory, Artifacts, snapshots, FTS5, automatic cross-provider routing, built-in OpenRouter, remote MCP/network effects, Tool PTYs/background jobs, worktrees, evaluation runtime, direct remote telemetry, OTLP logs and metrics are trigger-based future scope. A development PTY harness is terminal test infrastructure, not interactive agent command authority.

## Handoff

The initial architecture research is complete and the local implementation now has scoped synthetic evidence. [The documentation map](../README.md) locates current contract owners; [next steps](../../NEXT_STEPS.md) owns open defects, deferred functionality and final checks. New work follows [planning](../../planning/PLANNING.md), the [spec workflow](../specs/README.md), security controls and the existing testing admission rule. A research recommendation or accepted requirement is not evidence that its implementation or release gate passed.

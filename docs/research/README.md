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
| [Agent instructions and product specifications](./agent-instructions-and-product-specs-2026-10.md) | Conditional instruction loading, focused behavior specs, ADRs, Skills and evidence ownership |
| [Clean Architecture principles](./clean-architecture-principles-2026-10.md) | Martin's dependency rule and SOLID, interpreted against Arany's real ownership and privilege seams |
| [Current harness effect loops](./effectful-harness-tool-loops-2026-10.md) | Codex, Claude Code, Pi and OpenCode proposal/observation/recovery ownership |
| [MCP and runtime Skills](./mcp-and-runtime-skills-2026-10.md) | Current MCP versions, complete legacy tools flow, portable progressive Skills, bounded parser choices |
| [Platform effect Guard](./effect-guard-platform-base-2026-10.md) | Kernel filesystem/namespace/cgroup enforcement, lifecycle, quotas and immutable receipts |
| [macOS Guard crash recovery in other harnesses](./macos-guard-crash-recovery-in-harnesses-2026-10.md) | Owner-side cleanup versus persistent launchd/coalition/image recovery, source comparisons and verification limits |
| [Rust core feasibility](./rust-core-feasibility.md) | Engine language, storage, latency, async runtime, process protocol |
| [Agent-state persistence](./agent-state-persistence.md) | SQLite versus files, Rust KV stores, RocksDB, and PostgreSQL; crash/durability contract |
| [Rust foundation and Engine contract](./rust-foundation-and-engine-contract.md) | Runtime ownership, semantic Provider seam and blocking integration trade-offs |
| [CLI inputs and operability](./cli-inputs-configuration-and-operability.md) | Reasons for explicit modes, pinned inputs, billing selection and honest failures |
| [Testing strategy](./testing-strategy-for-rust-cli-harness.md) | Evidence-dense Rust/CLI testing, exact user output, SQLite replay, live Provider smoke, flake and deletion policy |
| [Existing-harness CLI feedback patterns](./cli-user-feedback-patterns-from-agent-harnesses.md) | Primary-source comparison of progress, attribution, approvals, replay, machine output, and failure UX |
| [Beta terminal interface and multi-agent feedback](./beta-terminal-interface-and-multi-agent-feedback.md) | Attached-human control room, deterministic automation, accessibility, terminal lifecycle, and PTY evidence |
| [Interactive CLI conventions and command surface](./interactive-cli-conventions-and-command-surface.md) | Entry-point conventions, startup flags, slash commands, parsing, lifecycle, and permission controls |
| [Beta Sessions, teams, terminal, providers, and license](./beta-sessions-teams-terminal-providers-and-license.md) | Footer/activity UX, mouse, bounded N, durable Sessions, custom endpoints, and attribution license |
| [Modular harness architecture](./modular-harness-architecture.md) | Deep modules, dependency direction, protocol/client seams, polyglot boundary |
| [Deterministic protection](./deterministic-harness-protection.md) | Capabilities, Policy, Guard, platform enforcement, Jev boundary |
| [Harness security lessons and controls](./harness-security-lessons-and-controls.md) | Primary-source failures in coding harnesses; V1 threat model, release blockers, non-claims, and future effect gates |
| [Instruction Markdown](./instruction-markdown-and-policy-enforcement.md) | `AGENTS.md`, `CLAUDE.md`, imports, typed restrict-only Policy |
| [Multi-agent loop and feedback](./multi-agent-loop-and-user-feedback.md) | Reusable loop, supervision, Assignment DAG, scheduling, user feedback |
| [Context, Memory, and compaction](./context-memory-and-compaction.md) | Context compiler, Session/Memory lifecycle, retrieval, caching, deletion |
| [Provider and tool runtime](./provider-and-tool-runtime.md) | Provider adapters, effect coordinator, processes, MCP, PTY/jobs |
| [Beta multi-provider routing and adapters](./beta-multi-provider-routing-and-adapters.md) | OpenAI, Anthropic, OpenRouter, and Z.AI capability, routing, privacy, and strict-output evidence |
| [Consumer subscription authentication](./consumer-subscription-authentication-for-provider-adapters.md) | OpenCode/Codex/Claude subscription flows, official policy, OAuth, secret lifecycle, and provider conformance |
| [October 2026 subscription route recheck](./subscription-provider-routes-2026-10.md) | Official ChatGPT open-source plan route and Anthropic third-party subscription restrictions |
| [Pi Anthropic subscription route check](./pi-anthropic-subscription-2026-10.md) | Pi's native OAuth implementation versus Anthropic's third-party authorization and billing boundary |
| [Evaluation and quality](./evaluation-and-quality-strategy.md) | Cases, trials, graders, baselines, release evidence |
| [OTLP observability](./otlp-observability.md) | Standard trace export, privacy, local Collector trust boundary, lifecycle, bounds, and evidence gates |
| [Next-step decision register](./next-step-decision-register.md) | Accepted architectural decisions, reasons and trade-offs |

## Maintenance

Keep source evidence and decision-relevant trade-offs. Remove superseded
recommendations, completed execution sequences and copied contracts; link their
current owners instead. Record proposed work in [planning](../../planning/README.md)
and completed checks in the handoff/PR and Git/CI history. Preserve scoped
security evidence when it still explains a risk or enforcement decision.

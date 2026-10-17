# Arany documentation

Start with the task, then read the owning contract and editing rules. [AGENTS.md](../AGENTS.md) routes development agents to mandatory rules; this guide locates product documentation. [NEXT_STEPS.md](../NEXT_STEPS.md) owns current implementation defects and the final verification handoff.

## Where each fact belongs

| Material | Authoritative home |
|---|---|
| Project introduction, goals, installation and development quickstart | [Repository README](../README.md); detailed contracts stay in their owners below |
| Human contribution workflow, issues and pull requests | [Contribution guide](../CONTRIBUTING.md) |
| Shared development commands and prerequisites | [Development guide](./development.md) |
| Shared development rules and conditional loading | [AGENTS.md](../AGENTS.md) |
| Implementation invariants and pitfalls | Tests that pin them; a one-line `//` comment only for a non-obvious reason beside the code; fixture procedures in [testing details](../agents/references/testing-details.md) |
| Agreed observable product behavior | [Domain specs](./specs/README.md); every domain has one |
| Physical architecture, data model and dependency direction | [System overview](./architecture/system-overview.md) |
| Architectural decisions, reasons and trade-offs | [Decision register](./research/next-step-decision-register.md), or a focused ADR when warranted |
| Source investigation, alternatives and research retention | [Research index](./research/README.md) and its [completion policy](./research/README.md#retention-and-completion) |
| Security findings and scoped supply-chain evidence | [Dependency inventory](./security/beta-dependency-inventory.md) and `docs/security/findings/` |
| Proposed changes and temporary execution work | [Planning](../planning/README.md) |
| Open defects, deferred functionality and user-owned checks | [Next steps](../NEXT_STEPS.md) |
| Completed changes and check results | Git/PR/CI history and the implementation handoff; no living chronological ledger |
| Testing design and trade-offs | [Testing strategy](./research/testing-strategy-for-rust-cli-harness.md); working requirements remain in [testing rules](../agents/testing.md) |
| User instructions | [Account setup](./setup.md), [local tools](./tools.md), [development diagnostics](./development-diagnostics.md) |
| Repeatable development procedures | [Project skills](../agents/skills/README.md) |
| Domain vocabulary | [CONTEXT.md](../CONTEXT.md) |

Specs state the agreed contract. Source and tests establish implementation and evidence; a mismatch must be reported and resolved rather than silently redefining the contract. Research recommendations and old passing checks do not grant runtime authority or establish current release support.

## Find the owner for a task

| Task | Read first | Related boundary |
|---|---|---|
| Terminal interaction, draft retention, selectors, accessibility or output | [Terminal spec](./specs/terminal.md) | Session and Tool specs for cross-boundary changes; [security rules](../agents/security.md) for untrusted rendering |
| Goal lifecycle, task budgets, plan contracts or recovery | [Goals spec](./specs/goals.md) | [Execution spec](./specs/execution.md), [Session spec](./specs/session.md); Provider and Tools for cross-boundary changes |
| Run coordination, teams, context or compaction | [Execution spec](./specs/execution.md), [Session spec](./specs/session.md) | Provider and terminal for cross-boundary changes |
| Provider, model or billing route | [Provider spec](./specs/provider.md) | [Credential spec](./specs/credentials.md), [ChatGPT spec](./specs/chatgpt.md), [security rules](../agents/security.md) |
| Saved accounts, OS-store authorization, storage or recovery | [Credential spec](./specs/credentials.md) | [ChatGPT spec](./specs/chatgpt.md), [Execution spec](./specs/execution.md#admission-persistence-and-recovery), [Terminal spec](./specs/terminal.md), [security rules](../agents/security.md) |
| Model-driven Tools, commands, Skills, MCP, Guard, folder trust or build cache | [Tool spec](./specs/tools.md), [tool guide](./tools.md) | Engine, Provider, Session, Execution and security |
| Trace export or local diagnostics | [Telemetry spec](./specs/telemetry.md), [Diagnostics spec](./specs/diagnostics.md) | Security and CLI admission |
| Behavior change, defect fix or release claim | The owning spec and [testing rules](../agents/testing.md) | Existing evidence owner and current handoff |
| Architecture or ownership change | [System overview](./architecture/system-overview.md) | Relevant domain specs and [planning workflow](../planning/PLANNING.md) |
| Development instructions or skill setup | [AGENTS.md](../AGENTS.md), [project skills](../agents/skills/README.md) | [Development commands](./development.md), [planning workflow](../planning/PLANNING.md); runtime Skill behavior remains in the Tool contract |

Follow relevant cross-boundary links before editing. A harmless prose correction does not require loading every contract. A linked file must actually be opened when its trigger applies; Markdown links and `@` text are not portable automatic imports.

## Read for the task

Read the owning contract and the rules [AGENTS.md](../AGENTS.md) triggers, then the implementation and its evidence owner. Open research or decision records only for rationale, checking their scope and date before reusing time-sensitive claims; consult a plan only for the initiative being continued. User guides explain actions, specs define outcomes, rules constrain edits and Skills describe repeatable procedures; link between them instead of copying. Spec requirements, implementation state and completed verification are distinct: report disagreements rather than choosing the newer document.

## Current milestone and evidence

The current milestone is a personal Linux beta. [Next steps](../NEXT_STEPS.md) owns open defects, unperformed checks and release work; [the agentic coding teams plan](../planning/agentic-teams/README.md) owns the remaining Beta 1 implementation work; accepted behavior belongs in specs. Keep specs independent of test totals, artifact hashes and past run logs.

The executable snapshots the exact Workspace-root AGENTS.md, with CLAUDE.md only as an absence fallback. This documentation workflow does not make Arany discover nested rules, follow spec links, or expand outbound content. Its admission contract remains in [Session spec](./specs/session.md) and [security rules](../agents/security.md).

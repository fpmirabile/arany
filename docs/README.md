# Arany documentation

Start with the task, then read the owning contract and editing rules. [AGENTS.md](../AGENTS.md) routes development agents to mandatory rules; this guide locates product documentation. [NEXT_STEPS.md](../NEXT_STEPS.md) owns current implementation defects and the final verification handoff.

## Where each fact belongs

| Material | Authoritative home |
|---|---|
| Project introduction, goals, installation and development quickstart | [Repository README](../README.md); keep detailed contracts and verification history in their owners below |
| Shared development rules and conditional loading | [AGENTS.md](../AGENTS.md) |
| Module editing constraints, ownership invariants and recurring failure modes | `agents/<module>.md`, reached through AGENTS.md; task-specific details are linked from each module under `agents/references/` |
| Agreed observable product behavior | [Domain specs](./specs/README.md); unmigrated contracts remain in their existing owners |
| Physical architecture, data model and dependency direction | [System overview](./architecture/system-overview.md) |
| Architectural decisions, reasons and trade-offs | [Decision register](./research/next-step-decision-register.md), or a focused ADR when warranted |
| Source investigation and alternatives | [Research index](./research/README.md) |
| Security findings and scoped supply-chain evidence | [Dependency inventory](./security/beta-dependency-inventory.md) and `docs/security/findings/` |
| Proposed changes and temporary execution work | [Planning](../planning/README.md) |
| Open defects, deferred functionality and user-owned checks | [Next steps](../NEXT_STEPS.md) |
| User instructions | [Account setup](./setup.md), [local tools](./tools.md), [development diagnostics](./development-diagnostics.md) |
| Repeatable development procedures | [Project skills](../agents/skills/README.md) |
| Domain vocabulary | [CONTEXT.md](../CONTEXT.md) |

Specs state the agreed contract. Source and tests establish implementation and evidence; a mismatch must be reported and resolved rather than silently redefining the contract. Research recommendations and old passing checks do not grant runtime authority or establish current release support.

## Find the owner for a task

| Task | Read first | Related boundary |
|---|---|---|
| Terminal interaction, draft retention, selectors, accessibility or output | [Terminal spec](./specs/terminal.md), [terminal rules](../agents/terminal.md) | [CLI](../agents/cli.md), [history](../agents/history.md); clipboard/image rules when involved |
| Session lifecycle, teams, context or compaction | [Execution spec](./specs/execution.md), [Session rules](../agents/session.md), [Engine rules](../agents/engine.md) | [Store](../agents/store.md), Provider and terminal for cross-boundary changes |
| Provider, model or billing route | [Provider rules](../agents/provider.md) | [Credential spec](./specs/credentials.md), [ChatGPT](../agents/chatgpt.md), [security](../agents/security.md) |
| Saved accounts, OS-store authorization, storage or recovery | [Credential spec](./specs/credentials.md), [credential rules](../agents/credentials.md) | [ChatGPT](../agents/chatgpt.md), [Store](../agents/store.md), [terminal](./specs/terminal.md), [security](../agents/security.md) |
| Model-driven Tools, commands, Skills or MCP | [Tool spec](./specs/tools.md), [Tool rules](../agents/tools.md), [Guard rules](../agents/guard.md), [tool guide](./tools.md) | Engine, Provider, Session, Store and security |
| Trace export or local diagnostics | [Diagnostic spec](./specs/diagnostics.md), [Telemetry rules](../agents/telemetry.md) or [diagnostic rules](../agents/diagnostics.md) | Security and CLI admission |
| Behavior change, defect fix or release claim | The owning contract and [testing rules](../agents/testing.md) | Existing evidence owner and current handoff |
| Architecture or ownership change | [System overview](./architecture/system-overview.md) | Relevant module rules and [planning workflow](../planning/PLANNING.md) |
| Development instructions or skill setup | [AGENTS.md](../AGENTS.md), [project skills](../agents/skills/README.md) | [Contribution guide](../CONTRIBUTING.md), [planning workflow](../planning/PLANNING.md); runtime Skill behavior remains in the Tool contract |

Follow relevant cross-boundary links before editing. A harmless prose correction does not require loading every contract. A linked file must actually be opened when its trigger applies; Markdown links and `@` text are not portable automatic imports.

## Current milestone and evidence

The current milestone is a personal Linux beta. [Next steps](../NEXT_STEPS.md) distinguishes a demonstrated defect from unperformed checks, missing functionality and beta-2/release work. [The active beta record](../planning/arany-beta/README.md), [next steps](../NEXT_STEPS.md) and scoped security/review records retain dated evidence. Completed execution plans are removed after their accepted behavior reaches the domain specs. Use the evidence owners for dated results; keep specs independent of test counts, artifact hashes and past run logs.

The executable snapshots the exact Workspace-root AGENTS.md, with CLAUDE.md only as an absence fallback. This documentation workflow does not make Arany discover nested rules, follow spec links, or expand outbound content. Its admission contract remains in [Session input rules](../agents/session.md) and [security rules](../agents/security.md).

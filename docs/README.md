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
| User instructions | [Account setup](./setup.md), [local tools](./tools.md), [development diagnostics](./specs/diagnostics.md) |
| Repeatable development procedures | [Project skills](../agents/skills/README.md) |
| Domain vocabulary | [CONTEXT.md](../CONTEXT.md) |

Specs state the agreed contract; source and tests establish implementation and evidence. Report and resolve a mismatch rather than silently redefining the contract or choosing the newer document. Research recommendations and old passing checks do not grant runtime authority or establish current release support.

## Find the owner for a task

[AGENTS.md](../AGENTS.md#rules-to-load-before-editing) maps each task or boundary to its owning spec and editing rules; [where each fact belongs](#where-each-fact-belongs) maps each kind of material to its home.

## Read for the task

Open research or decision records only for rationale, checking their scope and date before reusing time-sensitive claims; consult a plan only for the initiative being continued. User guides explain actions, specs define outcomes, rules constrain edits and Skills describe repeatable procedures; link between them instead of copying.

## Current milestone

The current milestone is a development beta on Linux and macOS. [Next steps](../NEXT_STEPS.md) owns open defects, unperformed checks and release work; [focused plans](../planning/README.md#active-plans) own proposed implementation work; accepted behavior belongs in specs. Keep specs independent of test totals, artifact hashes and past run logs.

Runtime instruction discovery belongs to the [Session contract](./specs/session.md#workspace-admission-and-authority);
documentation routing grants no authority or outbound-content expansion.

# Product specs

A spec is the maintained contract for one product domain: what a user or caller can observe, including rejection, recovery, limits and compatibility boundaries. It is independent of the implementation layout and is not an execution plan or a record of past test runs.

## Domain index

| Domain | Contract | Editing rules |
|---|---|---|
| Attached terminal, interactive controls, accessibility and deterministic output | [Terminal](./terminal.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Saved-account scope, storage, selected-item authorization and recovery | [Credentials](./credentials.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Tool grants, guarded file effects, commands, runtime Skills and local MCP | [Tools](./tools.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Durable task Goals, Run linkage, budgets, plan contracts and recovery | [Goals](./goals.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Bounded Run coordination, delegation, strict outcomes and history-capacity admission | [Execution](./execution.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Session lifecycle, Run and AgentRun identity, collaboration, context and compaction | [Session](./session.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Provider routes, adapters, models, strict outcomes, streaming and usage | [Provider](./provider.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| ChatGPT-plan OAuth, consent, tokens and catalogs | [ChatGPT](./chatgpt.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| OTLP export, privacy, bounds and shutdown | [Telemetry](./telemetry.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |
| Local development failure diagnostics, privacy and retention | [Diagnostics](./diagnostics.md) | [security](../../agents/security.md), [testing](../../agents/testing.md) |

Every product domain has a spec; `agents/` holds working practice only and implementation invariants live in tests. Create another spec when a real behavior change or an overloaded contract earns the extraction; avoid empty domain files.

## Maintain a contract

1. Behavior work starts from the owning contract: read it, name the successful path and the meaningful rejection, failure or recovery scenarios you change or add, and note their evidence owner. The request and recorded decisions set scope; clarify only material unspecified behavior.
2. Before closing, edit the contract only when agreed behavior, a limit, compatibility, recovery or scenario coverage changed or was missing. A refactor with unchanged behavior, or a fix that restores already specified behavior, needs no spec edit; add a regression scenario row only when the contract did not cover the case. Label accepted-but-unimplemented behavior and link its task. Proposals stay in `planning/`; open verification stays in `NEXT_STEPS.md`.
3. Reuse the owning test scenario or corpus and link it from the scenario table; [testing rules](../../agents/testing.md) decide when a new test is justified. Check local links after editing. In the PR, name the contract that changed when one did. Scenarios describe requirements, not a one-test-per-row layout.

When contract, code and evidence disagree, name the discrepancy and resolve it at the owner; neither an old research recommendation nor current faulty behavior silently overrides an agreed spec. Surface a conflict that recorded decisions cannot settle before changing the contract. Extract a new spec only when a real behavior change or an overloaded contract earns it; replace normative copies with links and update the domain index, the AGENTS.md loading trigger and inbound links together, then close the execution plan under the [planning lifecycle](../../planning/README.md#lifecycle).

## Spec structure

Use these sections when they contain substantive content; concise tables or prose are sufficient:

- **Scope and related contracts:** domain boundary, relevant vocabulary and authoritative dependencies.
- **Behavior:** observable successful operation, ordering, rejection, cancellation and recovery.
- **Limits and compatibility:** bounds whose meaning is public, overflow behavior and explicit non-claims. Reference code/configuration for incidental constants.
- **Acceptance scenarios and evidence owners:** concrete preconditions, action and expected outcome, with links to the existing tests or user procedure. Distinguish an owner from evidence that its gate passed.

Give scenarios stable headings or table names so plans and tests can reference them; describe semantics, not private function names or test helper mechanics. Specs express requirements, not verification status: name scenario owners here, keep open verification in `NEXT_STEPS.md`, and never keep a pass/fail or completed-work ledger in a spec. Native platforms, live Providers and human accessibility need their own evidence before a support claim.

## Extraction discipline

Route each passage by [where each fact belongs](../README.md#where-each-fact-belongs): observable behavior to the spec, editing procedure to general rules and implementation invariants to tests, data flow to architecture, rationale to the decision owner, proposals to a plan, completed verification to Git/PR/CI history and open checks to `NEXT_STEPS.md`. Validate with representative tasks (a terminal-copy edit reaches terminal behavior; a Provider decoder change reaches Provider, security and testing; a persistence change reaches Session, Store and testing). Extraction succeeds when agents locate the contract without losing a required boundary, not merely when a file gets shorter.

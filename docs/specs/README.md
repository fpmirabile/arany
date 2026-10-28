# Product specs

A spec is the maintained contract for one product domain: what a user or caller can observe, including rejection, recovery, limits and compatibility boundaries. It is independent of the implementation layout and is not an execution plan or a record of past test runs.

## Domain index

| Domain | Contract |
|---|---|
| `acp`, editor protocol and ACP capabilities | [ACP](./acp.md) |
| `serve --stdio`, JSON-RPC protocol and schema export | [Serve](./serve.md) |
| Attached terminal, interactive controls, accessibility and deterministic output | [Terminal](./terminal.md) |
| Saved-account scope, storage, selected-item authorization and recovery | [Credentials](./credentials.md) |
| Tool grants, guarded file effects, commands, runtime Skills and local MCP | [Tools](./tools.md) |
| Durable task Goals, Run linkage, budgets, plan contracts and recovery | [Goals](./goals.md) |
| Bounded Run coordination, delegation, strict outcomes and history-capacity admission | [Execution](./execution.md) |
| Session lifecycle, Run and AgentRun identity, collaboration, context and compaction | [Session](./session.md) |
| Provider routes, adapters, models, strict outcomes, streaming and usage | [Provider](./provider.md) |
| ChatGPT-plan OAuth, consent, tokens and catalogs | [ChatGPT](./chatgpt.md) |
| OTLP export, privacy, bounds and shutdown | [Telemetry](./telemetry.md) |
| Local development failure diagnostics, privacy and retention | [Diagnostics](./diagnostics.md) |

Every spec is edited under the [security](../../agents/security.md) and [testing](../../agents/testing.md) rules. Every product domain has a spec; `agents/` holds working practice only, and tests are the only evidence for a spec. Avoid empty domain files.

## Maintain a contract

1. Behavior work starts from the owning contract: read it and name the successful path and the meaningful rejection, failure or recovery behavior you change or add. The request and recorded decisions set scope; clarify only material unspecified behavior.
2. Edit the contract only when agreed behavior, a limit, compatibility or recovery changed or was missing. A refactor, or a fix that restores specified behavior, needs no spec edit. Label accepted-but-unimplemented behavior and link its task. Proposals stay in `planning/`; open verification stays in `NEXT_STEPS.md`.
3. An edit adds or replaces the owning sentence or section; it never restates behavior, a bound or a rationale another passage owns. Name the edited contract in the PR; a spec that grows for a refactor is a review finding.
4. Specs carry no scenario tables, test names or evidence links: the tests that prove a rule are found from the code. They also carry no per-version migration narratives ("N → M" steps) and no plan slice IDs, and they link a guarantee another passage owns instead of restating it. [Testing rules](../../agents/testing.md) decide when a new test is justified. Check local links after editing.

When contract, code and tests disagree, name the discrepancy and resolve it at the owner; neither an old research recommendation nor current faulty behavior silently overrides an agreed spec. Surface a conflict that recorded decisions cannot settle before changing the contract. Create a new spec only when a real behavior change or an overloaded contract earns it; replace normative copies with links, update the domain index, the AGENTS.md loading trigger and inbound links together, and close the execution plan under the [planning lifecycle](../../planning/README.md#lifecycle).

## Spec structure

Use these sections when they have substance; short prose or tables suffice:

- **Scope and related contracts:** domain boundary, vocabulary and authoritative dependencies.
- **Behavior:** observable success, ordering, rejection, cancellation and recovery, one rule per sentence.
- **Limits and compatibility:** bounds whose meaning is public, overflow behavior and explicit non-claims. Reference code or configuration for incidental constants. Link a bound with an exported Rust owner to its row in [the generated limits table](./limits.md), which `arany generate-limits` writes and is read-only.

Specs state requirements, not verification status: never keep a pass/fail or completed-work ledger in a spec. Native platforms, live Providers and human accessibility need their own evidence, tracked in `NEXT_STEPS.md`, before a support claim. Describe semantics, not private function names or test helpers.

### Generated bounds and disambiguation

`scripts/spec-bounds.py` rejects any prose value and unit matching a generated limit, including inline literals and table cells. Link the amount or bound name to its anchor in [limits.md](./limits.md), for example [the Tool result limit](./limits.md#max_tool_result_bytes). Numeric link labels must match the target row; unknown anchors fail. Fenced examples and HTML comments are not prose.

If an independent bound has the same value and unit, append `<!-- unrelated-bound: reason -->` right after that pair on the same line. The nonempty reason names the distinct bound or owner and covers only that pair. Never use it to exempt a restatement of an exported bound:

```markdown
64 KiB<!-- unrelated-bound: Git configuration file cap -->
```

## Extraction discipline

Route each passage by [where each fact belongs](../README.md#where-each-fact-belongs): observable behavior to the spec, editing procedure to general rules and implementation invariants to tests, data flow to architecture, rationale to the decision owner, proposals to a plan, completed verification to Git/PR/CI history and open checks to `NEXT_STEPS.md`. Validate with representative tasks (a terminal-copy edit reaches terminal behavior; a Provider decoder change reaches Provider, security and testing; a persistence change reaches Session, Store and testing). Extraction succeeds when agents find the contract without losing a required boundary, not when a file gets shorter.

# Product specs

A spec is the maintained contract for one product domain: what a user or caller can observe, including rejection, recovery, limits and compatibility boundaries. It is independent of the implementation layout and is not an execution plan or a record of past test runs.

## Domain index

| Domain | Contract | Editing rules |
|---|---|---|
| Attached terminal, interactive controls, accessibility and deterministic output | [Terminal](./terminal.md) | [Terminal](../../agents/terminal.md), [CLI](../../agents/cli.md), [history](../../agents/history.md) |

Only terminal has migrated. Other domains retain their current authoritative owners in [the documentation map](../README.md#find-the-owner-for-a-task). Create another spec when a real behavior change or an overloaded contract earns the extraction; avoid empty domain files.

## Maintain a contract

1. Identify whether the work affects observable behavior. For behavior work, read the owning contract and triggered editing rules, and identify its acceptance scenarios and existing evidence owner. An unmigrated domain's existing contract remains authoritative until extraction is warranted.
2. For new or changed behavior, describe the successful path and meaningful rejection, failure or recovery scenarios in the implementation plan when one is needed; otherwise put the accepted scenarios directly in the owning contract before implementation. The user's request and recorded decisions establish agreed scope; clarify only material unspecified behavior, rather than requesting approval again for already authorized work. Unaccepted alternatives remain proposals in the plan. A bug fix preserves the agreed behavior unless an authorized contract change applies.
3. After each planning pass and before closing any implementation, including work without a plan, compare the accepted decisions and resulting behavior with the owning contract. Update it when agreed behavior, limits, compatibility, recovery or scenario coverage changes or is missing. A bug fix that restores an adequately specified behavior needs no spec edit; add a missing regression scenario when the contract does not cover the case. A refactor with unchanged behavior needs no spec edit. For an unmigrated domain, extract a spec when the behavior change or overloaded contract warrants it, replacing the relevant normative copies with links. If accepted behavior remains unimplemented, label that gap and link its task. Keep unaccepted proposals in `planning/`, and current verification debt in `NEXT_STEPS.md`.
4. Reuse the owning test scenario or corpus. Link the spec's acceptance scenarios to that owner; [testing rules](../../agents/testing.md) decide when a new test is justified. Acceptance scenarios describe requirements, not a mandatory one-test-per-row layout.
5. Preserve security-critical editing constraints, architecture ownership and decision rationale in their respective homes. When extracting a spec, update the domain index, module loading trigger and inbound links together.
6. Check local links and anchors, changed prose and preservation of the contract. For behavior changes, run the repository checks required by the owning rules. Documentation-only changes need document/link review; they do not create new runtime evidence. In the existing plan, PR or final handoff, identify the owning contract and relevant evidence, and state what changed or why the contract remains adequate. For work with no observable impact, state that conclusion; no separate review log is required.

When contract, code and evidence disagree, name the discrepancy and resolve it at its owner. Neither an old research recommendation nor current faulty behavior silently overrides an agreed spec. If authoritative documents conflict and the decision is not recoverable from recorded user decisions, surface the uncertainty before changing the contract.

## Spec structure

Use these sections when they contain substantive content; concise tables or prose are sufficient:

- **Scope and related contracts:** domain boundary, relevant vocabulary and authoritative dependencies.
- **Behavior:** observable successful operation, ordering, rejection, cancellation and recovery.
- **Limits and compatibility:** bounds whose meaning is public, overflow behavior and explicit non-claims. Reference code/configuration for incidental constants.
- **Acceptance scenarios and evidence owners:** concrete preconditions, action and expected outcome, with links to the existing tests or user procedure. Distinguish an owner from evidence that its gate passed.

Give scenarios descriptive stable headings or table names so plans and tests can reference them. Use Given/When/Then wording when it clarifies a transition; no framework, generated schema or new identifier registry is required. Describe semantics rather than private function names, renderer libraries or test helper mechanics.

Contract status and verification status are separate. Specs express requirements; native platforms, live Providers and human accessibility need their own evidence before a support claim. Link dated outcomes and open checks to their existing records instead of maintaining a second pass/fail ledger here.

## Extraction discipline

Classify each passage before moving it:

| Passage | Destination |
|---|---|
| Observable behavior, bounds, failure and recovery | Domain spec |
| Editing procedure, privilege boundary, ownership invariant or recurring implementation trap | Module rules |
| Physical responsibility and data flow | Architecture |
| Context, alternatives and trade-offs | Existing decision record, or ADR when warranted |
| Proposed behavior or implementation sequence | Plan |
| Dated verification outcome or pending user check | Existing evidence record or next steps |

Validate the route with representative tasks: a terminal-copy edit reaches terminal behavior, a Provider decoder change still reaches Provider/security/testing, and a persistence change still reaches Session/Store/testing. Extraction succeeds when agents can locate the contract without losing required boundaries, not merely when a file becomes shorter.

# Agent instructions and product specifications

**Sources rechecked:** 2026-10-12

**Status:** research rationale for the maintained [documentation map](../README.md) and [spec workflow](../specs/README.md), not an additional instruction layer.

**Scope:** primary-source guidance for separating development instructions, product contracts, research and plans. No runtime instruction discovery or outbound-content expansion is introduced.

## Established patterns

There is useful convergence around concise instructions, scoped detail, and maintained specifications, but not one universal directory layout or loading mechanism.

- **A short instruction entry point with conditional references.** OpenAI recommends routing agents to architecture, database, or deployment documents when the corresponding task needs them, rather than loading every document before every edit. It also recommends narrow skill triggers and progressive disclosure. This is model-specific guidance, not permission to remove Arany's security or verification requirements. [OpenAI: rethinking skills and prompts](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra).
- **Scoped rules rather than eager imports.** Claude Code recommends concise global instructions and path-scoped rules for specialized work. Its `@path` imports expand into startup context: splitting a file through imports alone does not save context. Unscoped `.claude/rules/` files also load at launch; rules with matching `paths` load conditionally. Skills suit reusable procedures. [Claude Code memory and rules](https://code.claude.com/docs/en/memory).
- **Nested instructions where the client supports them.** The public AGENTS.md guidance recommends nested files for subprojects and treats the nearest applicable file as more specific. AGENTS.md itself is ordinary Markdown, not a standardized import engine. [AGENTS.md guidance](https://agents.md/).
- **Behavior specifications separate from execution plans.** Spec Kit separates feature requirements from technical planning and tasks, with project principles in a constitution. Its existing-project guide recommends starting with a bounded change, retaining existing conventions, and deciding whether completed specs remain historical or become living contracts. It does not require retrospectively specifying the whole application. [Spec Kit workflow](https://github.com/github/spec-kit#spec-driven-development), [existing-project guide](https://github.github.io/spec-kit/guides/existing-projects.html).
- **Current contracts separate from proposed changes.** OpenSpec keeps domain requirements and concrete scenarios in current specs, while proposed deltas live in change folders. Its maintainers explicitly recommend incremental adoption in existing repositories, not wholesale conversion of old documents. [OpenSpec concepts](https://github.com/Fission-AI/OpenSpec/blob/main/docs/concepts.md), [existing-project guide](https://github.com/Fission-AI/OpenSpec/blob/main/docs/existing-projects.md).
- **Small decision records for rationale.** Michael Nygard's ADR pattern records one consequential architectural decision with context, decision, status, and consequences; replaced decisions become superseded rather than silently losing their rationale. An ADR explains why, rather than replacing the current behavioral contract. [Documenting Architecture Decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).

## Human contribution guide and agent entry point

AGENTS.md's maintainers describe a dedicated entry point for coding agents that complements human-facing README and other documentation. This supports separating audiences, without requiring every technical fact to be copied into agent instructions. [AGENTS.md rationale](https://agents.md/#why-agentsmd).

GitHub presents CONTRIBUTING.md as guidance for people preparing useful issues and pull requests, and surfaces it in those contribution flows. Its Open Source Guides recommend welcoming explanations of participation, bug reports, feature proposals, development setup and tests. Human-facing therefore does not mean nontechnical. [GitHub contributor guidelines](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors), [Open Source Guides](https://opensource.guide/starting-a-project/#writing-your-contributing-guidelines).

**Application to Arany:** the user's accepted preference is that humans start with CONTRIBUTING.md and agents start with AGENTS.md. Agent routing should reach applicable rules and verification directly, without making the human contribution guide a mandatory intermediate read. Shared setup, commands and checks can still have one technical owner linked from both entry points. This is consistent with the cited audience guidance; the sources do not establish a universal ban on agents reading CONTRIBUTING.md or a requirement to maintain separate copies of shared procedures.

## Discovery is not a Markdown link

Codex documents a startup instruction chain from the project root to the current working directory, with configured size limits and filename precedence. That is not a promise that every nested file below a root session will be dynamically loaded for every later edit. [Codex AGENTS.md guide](https://learn.chatgpt.com/docs/agent-configuration/agents-md).

Claude Code's current AGENTS.md support depends on version and competing CLAUDE.md files; its on-demand discovery and import handling are client-specific. A prose request or ordinary Markdown link still requires the agent to open the target. Do not assume `@` imports or nested discovery behave identically across tools. [Claude Code instruction loading](https://code.claude.com/docs/en/memory#agentsmd).

The Agent Skills standard explicitly describes progressive loading: metadata first, the selected skill body next, supporting resources as needed. Merely moving a large rule file into a skill would not make its body smaller once activated. [Agent Skills specification](https://agentskills.io/specification#progressive-disclosure).

## What effectiveness evidence does and does not show

Vendor authoring advice is not a controlled performance result. Two primary studies report different outcomes:

- Gloaguen et al. found that context files did not generally improve task success and increased average inference cost by over 20% across their evaluated agents and repository tasks. Agents followed the instructions, but repository overviews did not help; the authors distinguish useful non-standard practices from unverified performance optimization. [Evaluating AGENTS.md, v3](https://arxiv.org/abs/2602.11988v3).
- Lulla et al. studied 124 pull requests from ten repositories and reported 28.64% lower median runtime and 16.58% lower output-token consumption with AGENTS.md, with comparable completion behavior. Those efficiency observations do not establish better correctness or transfer to Arany. [On the Impact of AGENTS.md Files, v2](https://arxiv.org/abs/2601.20404v2).

These studies use different tasks and evaluation setups. Neither establishes a universal line limit, optimal directory tree or measured benefit for this repository. **Inference for Arany:** retain non-obvious invariants and task routing, remove duplicated or stale instructions, and evaluate a claimed performance improvement on representative tasks before describing it as proven. The current documentation audit verifies ownership, links and loading routes; it does not measure agent success, latency or token savings.

## Adopted direction for Arany

The repository already has the right partial separation: [AGENTS.md](../../AGENTS.md) routes to module instructions and identifies [the architecture overview](../architecture/system-overview.md) as canonical context. Refine this organization rather than installing a new framework or replacing every rule with a giant spec.

| Material | Narrow authoritative home | Purpose |
|---|---|---|
| Repository-wide guardrails and task routing | `AGENTS.md` | How an agent works and when to read more |
| Module-specific editing constraints and recurring gotchas | `agents/<module>.md` | What a developer must preserve while changing that module |
| Durable product behavior | `docs/specs/<domain>.md`; unmigrated domains retain their mapped owner | Successful behavior, rejection behavior, bounds, recovery, and non-claims |
| Physical ownership and dependency structure | Existing `docs/architecture/` | How the implementation is organized |
| Consequential trade-offs and rejected alternatives | Existing decision register; focused ADRs when needed | Why an architectural decision was made |
| Source investigation and alternatives | Existing `docs/research/` | Dated evidence, uncertainty and decision-relevant comparisons |
| Repeatable development procedures | Existing development Skills | How to perform a particular task |
| Temporary tasks and verification handoff | Existing `planning/` and `NEXT_STEPS.md` | What remains to be done |

The [spec index](../specs/README.md) identifies the migrated domains and their acceptance evidence owners. Module rules retain editing invariants and recurring failure modes; architecture retains physical responsibility. A research recommendation is not an accepted requirement, and an accepted requirement is not proof of implementation. An adequate existing owner needs no new spec merely to make the directory tree uniform.

[The spec workflow](../specs/README.md) owns the maintained procedure and acceptance criteria. Proposals stay in plans; accepted behavior and its implementation change together, with explicit gaps when incomplete. Security-critical editing constraints and runtime authority remain unchanged. The existing decision register retains rationale rather than creating ADRs for every choice.

This organization concerns development-agent documentation. Arany itself snapshots the exact Workspace-root `AGENTS.md`, with exact root `CLAUDE.md` only as an absence fallback; that is a separate contract from Codex or Claude Code discovery. Moving guidance into specs does not make the executable automatically follow links, `@` imports, or nested instructions. A migration must not expand included outbound content, Provider admission, credential access, Policy, or Guard authority. [Current Arany architecture](../architecture/system-overview.md#1-what-the-beta-must-prove).

The repository uses plain Markdown and existing plans/tests. The audit checks representative routes: a terminal change reaches its spec and editing rules; a credential change reaches consent, storage and security owners; a prose correction avoids unrelated runtime rules; a research task distinguishes evidence from accepted decisions. Framework automation remains unnecessary unless maintenance becomes a demonstrated problem.
